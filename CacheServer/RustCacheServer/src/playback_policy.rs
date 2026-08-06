use std::fmt;

use crate::{
    bbdown_adapter::{BilibiliMediaRequest, BilibiliPlaybackVariant, BilibiliPlaybackVariantKind},
    codecs::{codec_list_matches, is_aac_codec, is_h264_codec},
    generated::tvos_net_player::v1::{
        BilibiliCompatibleVariantPreference as ProtoCompatibleVariantPreference,
        BilibiliPlaybackOptions, BilibiliPlaybackPolicy as ProtoBilibiliPlaybackPolicy,
        BilibiliTranscodingPreference as ProtoTranscodingPreference,
        BilibiliWeakNetworkPreference as ProtoWeakNetworkPreference,
    },
    transcoding::{
        LAN_TRANSCODING_AUDIO_BANDWIDTH_BPS, LAN_TRANSCODING_MAX_FRAME_RATE,
        LAN_TRANSCODING_MAX_HEIGHT, LAN_TRANSCODING_MAX_VIDEO_BANDWIDTH_BPS,
        LAN_TRANSCODING_MAX_WIDTH,
    },
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct PlaybackPolicy {
    pub(crate) transcoding_preference: TranscodingPreference,
    pub(crate) compatible_variant_preference: CompatibleVariantPreference,
    pub(crate) weak_network_preference: WeakNetworkPreference,
}

impl PlaybackPolicy {
    pub(crate) fn from_playback_options(
        options: Option<&BilibiliPlaybackOptions>,
    ) -> Result<Self, PlaybackPolicyError> {
        Self::from_proto(options.and_then(|options| options.playback_policy.as_ref()))
    }

    pub(crate) fn from_proto(
        policy: Option<&ProtoBilibiliPlaybackPolicy>,
    ) -> Result<Self, PlaybackPolicyError> {
        let Some(policy) = policy else {
            return Ok(Self::default());
        };
        Ok(Self {
            transcoding_preference: TranscodingPreference::from_proto_i32(
                policy.transcoding_preference,
            )
            .map_err(|value| PlaybackPolicyError::new("transcoding_preference", value))?,
            compatible_variant_preference: CompatibleVariantPreference::from_proto_i32(
                policy.compatible_variant_preference,
            )
            .map_err(|value| PlaybackPolicyError::new("compatible_variant_preference", value))?,
            weak_network_preference: WeakNetworkPreference::from_proto_i32(
                policy.weak_network_preference,
            )
            .map_err(|value| PlaybackPolicyError::new("weak_network_preference", value))?,
        })
    }

    pub(crate) fn to_proto(self) -> ProtoBilibiliPlaybackPolicy {
        ProtoBilibiliPlaybackPolicy {
            transcoding_preference: self.transcoding_preference.to_proto().into(),
            compatible_variant_preference: self.compatible_variant_preference.to_proto().into(),
            weak_network_preference: self.weak_network_preference.to_proto().into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlaybackPolicyError {
    field: &'static str,
    value: i32,
}

impl PlaybackPolicyError {
    fn new(field: &'static str, value: i32) -> Self {
        Self { field, value }
    }
}

impl fmt::Display for PlaybackPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Unknown Bilibili playback policy value {} for {}.",
            self.value, self.field
        )
    }
}

impl std::error::Error for PlaybackPolicyError {}

impl Default for PlaybackPolicy {
    fn default() -> Self {
        Self {
            transcoding_preference: TranscodingPreference::Auto,
            compatible_variant_preference: CompatibleVariantPreference::PreferCompatible,
            weak_network_preference: WeakNetworkPreference::Adaptive,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranscodingPreference {
    #[default]
    Auto,
    Never,
    Force,
}

impl TranscodingPreference {
    fn from_proto_i32(value: i32) -> Result<Self, i32> {
        match ProtoTranscodingPreference::try_from(value) {
            Ok(ProtoTranscodingPreference::Never) => Ok(Self::Never),
            Ok(ProtoTranscodingPreference::Force) => Ok(Self::Force),
            Ok(ProtoTranscodingPreference::Unspecified | ProtoTranscodingPreference::Auto) => {
                Ok(Self::Auto)
            }
            Err(_) => Err(value),
        }
    }

    fn to_proto(self) -> ProtoTranscodingPreference {
        match self {
            Self::Auto => ProtoTranscodingPreference::Auto,
            Self::Never => ProtoTranscodingPreference::Never,
            Self::Force => ProtoTranscodingPreference::Force,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompatibleVariantPreference {
    #[default]
    PreferCompatible,
    PreferRequested,
}

impl CompatibleVariantPreference {
    fn from_proto_i32(value: i32) -> Result<Self, i32> {
        match ProtoCompatibleVariantPreference::try_from(value) {
            Ok(ProtoCompatibleVariantPreference::PreferRequested) => Ok(Self::PreferRequested),
            Ok(
                ProtoCompatibleVariantPreference::Unspecified
                | ProtoCompatibleVariantPreference::PreferCompatible,
            ) => Ok(Self::PreferCompatible),
            Err(_) => Err(value),
        }
    }

    fn to_proto(self) -> ProtoCompatibleVariantPreference {
        match self {
            Self::PreferCompatible => ProtoCompatibleVariantPreference::PreferCompatible,
            Self::PreferRequested => ProtoCompatibleVariantPreference::PreferRequested,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WeakNetworkPreference {
    #[default]
    Adaptive,
    HoldDowngrade,
    AvPlayerManaged,
}

impl WeakNetworkPreference {
    fn from_proto_i32(value: i32) -> Result<Self, i32> {
        match ProtoWeakNetworkPreference::try_from(value) {
            Ok(ProtoWeakNetworkPreference::HoldDowngrade) => Ok(Self::HoldDowngrade),
            Ok(ProtoWeakNetworkPreference::AvplayerManaged) => Ok(Self::AvPlayerManaged),
            Ok(ProtoWeakNetworkPreference::Unspecified | ProtoWeakNetworkPreference::Adaptive) => {
                Ok(Self::Adaptive)
            }
            Err(_) => Err(value),
        }
    }

    fn to_proto(self) -> ProtoWeakNetworkPreference {
        match self {
            Self::Adaptive => ProtoWeakNetworkPreference::Adaptive,
            Self::HoldDowngrade => ProtoWeakNetworkPreference::HoldDowngrade,
            Self::AvPlayerManaged => ProtoWeakNetworkPreference::AvplayerManaged,
        }
    }
}

pub(crate) fn variant_is_avplayer_h264_aac_hls_compatible(
    variant: &BilibiliPlaybackVariant,
) -> bool {
    variant_has_avplayer_h264_aac_hls_codecs(variant)
        && variant.video.as_ref().is_some_and(|video| {
            variant_is_within_transcoding_profile_envelope(variant, video)
                && variant_h264_level_is_within_transcoding_profile(variant, video)
        })
}

pub(crate) fn variant_has_avplayer_h264_aac_hls_codecs(variant: &BilibiliPlaybackVariant) -> bool {
    if variant.kind != BilibiliPlaybackVariantKind::Dash {
        return false;
    }
    let Some(video) = variant.video.as_ref() else {
        return false;
    };
    variant_has_codec(variant, Some(video), is_h264_codec)
        && variant
            .audio
            .as_ref()
            .is_none_or(|audio| variant_has_codec(variant, Some(audio), is_aac_codec))
}

fn variant_is_within_transcoding_profile_envelope(
    variant: &BilibiliPlaybackVariant,
    video: &BilibiliMediaRequest,
) -> bool {
    bounded_u32_with_evidence(variant.width, video.width, LAN_TRANSCODING_MAX_WIDTH)
        && bounded_u32_with_evidence(variant.height, video.height, LAN_TRANSCODING_MAX_HEIGHT)
        && bounded_frame_rate_with_evidence(
            variant.frame_rate.as_deref(),
            video.frame_rate.as_deref(),
        )
        && bounded_bandwidth_with_evidence(variant, video)
}

fn transcoding_profile_total_bandwidth(has_audio: bool) -> u64 {
    LAN_TRANSCODING_MAX_VIDEO_BANDWIDTH_BPS
        + if has_audio {
            LAN_TRANSCODING_AUDIO_BANDWIDTH_BPS
        } else {
            0
        }
}

fn bounded_u32_with_evidence(first: Option<u32>, second: Option<u32>, max: u32) -> bool {
    let values = [first, second];
    values.iter().flatten().next().is_some()
        && values
            .into_iter()
            .flatten()
            .all(|value| value > 0 && value <= max)
}

fn bounded_bandwidth_with_evidence(
    variant: &BilibiliPlaybackVariant,
    video: &BilibiliMediaRequest,
) -> bool {
    let audio_bandwidth = variant.audio.as_ref().and_then(|audio| audio.bandwidth);
    let has_component_evidence =
        video.bandwidth.is_some() && (variant.audio.is_none() || audio_bandwidth.is_some());
    (variant.bandwidth.is_some() || has_component_evidence)
        && bounded_optional_u64(
            variant.bandwidth,
            transcoding_profile_total_bandwidth(variant.audio.is_some()),
        )
        && bounded_optional_u64(video.bandwidth, LAN_TRANSCODING_MAX_VIDEO_BANDWIDTH_BPS)
        && bounded_optional_u64(audio_bandwidth, LAN_TRANSCODING_AUDIO_BANDWIDTH_BPS)
}

fn bounded_optional_u64(value: Option<u64>, max: u64) -> bool {
    value.is_none_or(|value| value > 0 && value <= max)
}

fn bounded_frame_rate_with_evidence(first: Option<&str>, second: Option<&str>) -> bool {
    let mut saw_evidence = false;
    for frame_rate in [first, second].into_iter().flatten() {
        saw_evidence = true;
        let Some(rate) = parse_frame_rate(frame_rate.trim()) else {
            return false;
        };
        if rate <= 0.0 || rate > LAN_TRANSCODING_MAX_FRAME_RATE {
            return false;
        }
    }
    saw_evidence
}

fn parse_frame_rate(frame_rate: &str) -> Option<f64> {
    if let Some((numerator, denominator)) = frame_rate.split_once('/') {
        let numerator = numerator.trim().parse::<f64>().ok()?;
        let denominator = denominator.trim().parse::<f64>().ok()?;
        if denominator <= 0.0 {
            return None;
        }
        let parsed = numerator / denominator;
        return parsed.is_finite().then_some(parsed);
    }
    frame_rate
        .parse::<f64>()
        .ok()
        .filter(|rate| rate.is_finite())
}

fn variant_h264_level_is_within_transcoding_profile(
    variant: &BilibiliPlaybackVariant,
    video: &BilibiliMediaRequest,
) -> bool {
    if let Some(codecs) = video.codecs.as_deref() {
        return codec_list_h264_is_within_transcoding_profile(codecs);
    }

    let mut saw_h264 = false;
    for codecs in &variant.codecs {
        for codec in codecs.split(',').filter(|codec| is_h264_codec(codec)) {
            saw_h264 = true;
            if !h264_codec_is_within_transcoding_profile(codec) {
                return false;
            }
        }
    }
    saw_h264
}

fn codec_list_h264_is_within_transcoding_profile(codecs: &str) -> bool {
    let mut saw_h264 = false;
    for codec in codecs.split(',').filter(|codec| is_h264_codec(codec)) {
        saw_h264 = true;
        if !h264_codec_is_within_transcoding_profile(codec) {
            return false;
        }
    }
    saw_h264
}

fn h264_codec_is_within_transcoding_profile(codec: &str) -> bool {
    let codec = codec.trim().to_ascii_lowercase();
    let Some(profile_level_id) = codec
        .strip_prefix("avc1.")
        .or_else(|| codec.strip_prefix("avc3."))
        .and_then(|value| value.split('.').next())
    else {
        return false;
    };
    if profile_level_id.len() != 6
        || !profile_level_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }

    let profile_idc = u8::from_str_radix(&profile_level_id[0..2], 16).ok();
    let level_idc = u8::from_str_radix(&profile_level_id[4..6], 16).ok();
    profile_idc.is_some_and(h264_profile_is_within_transcoding_profile)
        && level_idc.is_some_and(|level| level <= 0x2A)
}

fn h264_profile_is_within_transcoding_profile(profile_idc: u8) -> bool {
    matches!(profile_idc, 0x42 | 0x4D | 0x64)
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
    use crate::generated::tvos_net_player::v1::BilibiliPlaybackPolicy;

    #[test]
    fn unspecified_policy_values_normalize_to_safe_defaults() {
        let policy = PlaybackPolicy::from_proto(Some(&BilibiliPlaybackPolicy {
            transcoding_preference: ProtoTranscodingPreference::Unspecified.into(),
            compatible_variant_preference: ProtoCompatibleVariantPreference::Unspecified.into(),
            weak_network_preference: ProtoWeakNetworkPreference::Unspecified.into(),
        }))
        .expect("unspecified policy values should be accepted");

        assert_eq!(PlaybackPolicy::default(), policy);
        let proto = policy.to_proto();
        assert_eq!(
            ProtoTranscodingPreference::Auto,
            proto.transcoding_preference()
        );
        assert_eq!(
            ProtoCompatibleVariantPreference::PreferCompatible,
            proto.compatible_variant_preference()
        );
        assert_eq!(
            ProtoWeakNetworkPreference::Adaptive,
            proto.weak_network_preference()
        );
    }

    #[test]
    fn preserves_explicit_non_default_policy_values() {
        let policy = PlaybackPolicy::from_proto(Some(&BilibiliPlaybackPolicy {
            transcoding_preference: ProtoTranscodingPreference::Force.into(),
            compatible_variant_preference: ProtoCompatibleVariantPreference::PreferRequested.into(),
            weak_network_preference: ProtoWeakNetworkPreference::HoldDowngrade.into(),
        }))
        .expect("known policy values should be accepted");

        assert_eq!(TranscodingPreference::Force, policy.transcoding_preference);
        assert_eq!(
            CompatibleVariantPreference::PreferRequested,
            policy.compatible_variant_preference
        );
        assert_eq!(
            WeakNetworkPreference::HoldDowngrade,
            policy.weak_network_preference
        );
    }

    #[test]
    fn rejects_unknown_policy_enum_values() {
        let cases = [
            (
                "transcoding_preference",
                BilibiliPlaybackPolicy {
                    transcoding_preference: 99,
                    ..BilibiliPlaybackPolicy::default()
                },
            ),
            (
                "compatible_variant_preference",
                BilibiliPlaybackPolicy {
                    compatible_variant_preference: 99,
                    ..BilibiliPlaybackPolicy::default()
                },
            ),
            (
                "weak_network_preference",
                BilibiliPlaybackPolicy {
                    weak_network_preference: 99,
                    ..BilibiliPlaybackPolicy::default()
                },
            ),
        ];

        for (field, proto) in cases {
            let error = PlaybackPolicy::from_proto(Some(&proto))
                .expect_err("unknown policy enum value should be rejected");
            assert_eq!(field, error.field);
            assert_eq!(99, error.value);
        }
    }
}
