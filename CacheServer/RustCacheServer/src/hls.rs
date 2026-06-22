use std::{
    collections::HashMap,
    fmt::{self, Display},
    sync::{Arc, RwLock},
};

use crate::bbdown_adapter::{
    BilibiliMediaCacheKey, BilibiliMediaRequest, BilibiliMediaRequestKind,
    BilibiliPlaybackAbrGroupKind as AdapterAbrGroupKind,
    BilibiliPlaybackAbrLevel as AdapterAbrLevel, BilibiliPlaybackAbrMetadata as AdapterAbrMetadata,
    BilibiliPlaybackVariant as AdapterPlaybackVariant, BilibiliPlaybackVariantKind,
};
use url::Url;

const VIDEO_SEGMENT_ID: &str = "video.m4s";
const AUDIO_SEGMENT_ID: &str = "audio.m4s";
const DEFAULT_BANDWIDTH: u64 = 1_000_000;
const DEFAULT_DURATION_SECONDS: u32 = 1;

#[derive(Clone, Default)]
pub(crate) struct HlsPlaybackRegistry {
    inner: Arc<RwLock<HashMap<String, HlsPlaybackSession>>>,
}

impl HlsPlaybackRegistry {
    pub(crate) fn insert(&self, session: HlsPlaybackSession) {
        self.inner
            .write()
            .expect("HLS playback registry lock poisoned")
            .insert(session.id.clone(), session);
    }

    pub(crate) fn remove(&self, session_id: &str) {
        self.inner
            .write()
            .expect("HLS playback registry lock poisoned")
            .remove(session_id);
    }

    pub(crate) fn get(&self, session_id: &str) -> Option<HlsPlaybackSession> {
        self.inner
            .read()
            .expect("HLS playback registry lock poisoned")
            .get(session_id)
            .cloned()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsPlaybackSession {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) variant: HlsVariant,
    pub(crate) alternate_variants: Vec<HlsVariant>,
    pub(crate) abr: HlsAbrMetadata,
    pub(crate) variants: Vec<HlsVariantMetadata>,
}

impl HlsPlaybackSession {
    pub(crate) fn from_selected_variant(
        session_id: &str,
        title: &str,
        variant: &AdapterPlaybackVariant,
    ) -> Result<Self, HlsSessionError> {
        if variant.kind != BilibiliPlaybackVariantKind::Dash {
            return Err(HlsSessionError::new(
                "Progressive HLS playback requires a DASH playback variant.",
            ));
        }

        let hls_variant = HlsVariant::from_adapter(variant)?;
        Ok(Self {
            id: session_id.to_owned(),
            title: title.to_owned(),
            variant: hls_variant,
            alternate_variants: Vec::new(),
            abr: HlsAbrMetadata::default(),
            variants: vec![HlsVariantMetadata::from_adapter(variant)],
        })
    }

    pub(crate) fn from_playback_entry(
        session_id: &str,
        title: &str,
        selected_variant: &AdapterPlaybackVariant,
        abr: &AdapterAbrMetadata,
        variants: &[AdapterPlaybackVariant],
    ) -> Result<Self, HlsSessionError> {
        let mut session = Self::from_selected_variant(session_id, title, selected_variant)?;
        session.abr = HlsAbrMetadata::from_adapter(abr);
        session.variants = variants
            .iter()
            .map(HlsVariantMetadata::from_adapter)
            .collect();
        session.alternate_variants = playable_alternate_variants(selected_variant, variants)?;
        if !session
            .variants
            .iter()
            .any(|variant| variant.id == session.variant.id)
        {
            session
                .variants
                .push(HlsVariantMetadata::from_adapter(selected_variant));
        }
        Ok(session)
    }

    pub(crate) fn master_playlist(&self) -> String {
        let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
        let variants = self.playable_variants().collect::<Vec<_>>();
        for (index, variant) in variants.iter().enumerate() {
            let Some(audio_playlist_id) = variant.audio_playlist_id() else {
                continue;
            };
            let group_id = audio_group_id(index);
            let name = if index == 0 {
                "Default".to_owned()
            } else {
                format!("Variant {index}")
            };
            let default = if index == 0 { "YES" } else { "NO" };
            playlist.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{}\",NAME=\"{}\",DEFAULT={default},AUTOSELECT=YES,URI=\"segments/{}\"\n",
                escape_quoted(&group_id),
                escape_quoted(&name),
                escape_quoted(&audio_playlist_id)
            ));
        }

        for (index, variant) in variants.iter().enumerate() {
            playlist.push_str("#EXT-X-STREAM-INF:");
            playlist.push_str(&format!("BANDWIDTH={}", variant.bandwidth));
            if let Some(resolution) = variant.resolution() {
                playlist.push_str(&format!(",RESOLUTION={resolution}"));
            }
            if let Some(codecs) = variant.codecs_attribute() {
                playlist.push_str(&format!(",CODECS=\"{}\"", escape_quoted(&codecs)));
            }
            if variant.audio.is_some() {
                playlist.push_str(&format!(
                    ",AUDIO=\"{}\"",
                    escape_quoted(&audio_group_id(index))
                ));
            }
            playlist.push_str(&format!(
                "\nsegments/{}\n",
                escape_quoted(&variant.video_playlist_id())
            ));
        }
        playlist
    }

    pub(crate) fn media_playlist_resource(&self, playlist_id: &str) -> Option<HlsMediaResource> {
        self.media_playlist_resource_ref(playlist_id)
            .map(|(_, resource)| resource.clone())
    }

    pub(crate) fn media_playlist(
        &self,
        playlist_id: &str,
        initialization_length: u64,
        total_length: u64,
    ) -> Option<String> {
        let (variant, resource) = self.media_playlist_resource_ref(playlist_id)?;
        let media_length = total_length.checked_sub(initialization_length)?;
        if initialization_length == 0 || media_length == 0 {
            return None;
        }
        let duration = resource
            .request
            .duration_seconds
            .unwrap_or(variant.duration_seconds)
            .max(DEFAULT_DURATION_SECONDS);

        Some(format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:{duration}\n#EXT-X-MAP:URI=\"{}\",BYTERANGE=\"{initialization_length}@0\"\n#EXTINF:{duration}.000,\n#EXT-X-BYTERANGE:{media_length}@{initialization_length}\n{}\n#EXT-X-ENDLIST\n",
            resource.id, resource.id
        ))
    }

    pub(crate) fn media_resource(&self, segment_id: &str) -> Option<HlsMediaResource> {
        for variant in self.playable_variants() {
            if segment_id == variant.video.id {
                return Some(variant.video.clone());
            }

            if let Some(audio) = variant
                .audio
                .as_ref()
                .filter(|audio| segment_id == audio.id)
            {
                return Some(audio.clone());
            }
        }
        None
    }

    fn playable_variants(&self) -> impl Iterator<Item = &HlsVariant> {
        std::iter::once(&self.variant).chain(self.alternate_variants.iter())
    }

    fn media_playlist_resource_ref(
        &self,
        playlist_id: &str,
    ) -> Option<(&HlsVariant, &HlsMediaResource)> {
        for variant in self.playable_variants() {
            if playlist_id == variant.video_playlist_id() {
                return Some((variant, &variant.video));
            }
            if let Some(audio) = &variant.audio
                && variant
                    .audio_playlist_id()
                    .is_some_and(|audio_playlist_id| playlist_id == audio_playlist_id)
            {
                return Some((variant, audio));
            }
        }
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsVariant {
    pub(crate) id: String,
    pub(crate) bandwidth: u64,
    pub(crate) codecs: Vec<String>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) duration_seconds: u32,
    pub(crate) video: HlsMediaResource,
    pub(crate) audio: Option<HlsMediaResource>,
}

impl HlsVariant {
    fn from_adapter(variant: &AdapterPlaybackVariant) -> Result<Self, HlsSessionError> {
        Self::from_adapter_with_resource_ids(
            variant,
            VIDEO_SEGMENT_ID.to_owned(),
            AUDIO_SEGMENT_ID.to_owned(),
        )
    }

    fn from_adapter_with_resource_ids(
        variant: &AdapterPlaybackVariant,
        video_id: String,
        audio_id: String,
    ) -> Result<Self, HlsSessionError> {
        let Some(video) = variant.video.clone() else {
            return Err(HlsSessionError::new(
                "Progressive HLS playback requires a video media request.",
            ));
        };

        let audio = variant.audio.clone();
        let codecs = hls_variant_codecs(variant, &video, audio.as_ref());
        Ok(Self {
            id: variant.id.clone(),
            bandwidth: variant
                .bandwidth
                .or(video.bandwidth)
                .unwrap_or(DEFAULT_BANDWIDTH),
            codecs,
            width: variant.width,
            height: variant.height,
            duration_seconds: variant
                .duration_seconds
                .or(video.duration_seconds)
                .unwrap_or(DEFAULT_DURATION_SECONDS),
            video: HlsMediaResource {
                id: video_id,
                request: video,
            },
            audio: audio.map(|audio| HlsMediaResource {
                id: audio_id,
                request: audio,
            }),
        })
    }

    fn resolution(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(width), Some(height)) => Some(format!("{width}x{height}")),
            _ => None,
        }
    }

    fn codecs_attribute(&self) -> Option<String> {
        let mut codecs = Vec::new();
        for codec in &self.codecs {
            push_unique_codec(&mut codecs, codec);
        }
        if let Some(audio_codecs) = self
            .audio
            .as_ref()
            .and_then(|audio| audio.request.codecs.as_ref())
        {
            push_unique_codec(&mut codecs, audio_codecs);
        }
        (!codecs.is_empty()).then(|| codecs.join(","))
    }

    fn video_playlist_id(&self) -> String {
        media_playlist_id(&self.video.id)
    }

    fn audio_playlist_id(&self) -> Option<String> {
        self.audio
            .as_ref()
            .map(|audio| media_playlist_id(&audio.id))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct HlsAbrMetadata {
    pub(crate) groups: Vec<HlsAbrGroup>,
}

impl HlsAbrMetadata {
    fn from_adapter(metadata: &AdapterAbrMetadata) -> Self {
        Self {
            groups: metadata
                .groups
                .iter()
                .map(HlsAbrGroup::from_adapter)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsAbrGroup {
    pub(crate) id: String,
    pub(crate) kind: HlsAbrGroupKind,
    pub(crate) variant_ids: Vec<String>,
    pub(crate) level_count: u32,
    pub(crate) min_bandwidth: Option<u64>,
    pub(crate) max_bandwidth: Option<u64>,
}

impl HlsAbrGroup {
    fn from_adapter(group: &crate::bbdown_adapter::BilibiliPlaybackAbrGroup) -> Self {
        Self {
            id: group.id.clone(),
            kind: HlsAbrGroupKind::from_adapter(group.kind),
            variant_ids: group.variant_ids.clone(),
            level_count: group.level_count,
            min_bandwidth: group.min_bandwidth,
            max_bandwidth: group.max_bandwidth,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsAbrGroupKind {
    DashVideo,
    DashAudioOnly,
}

impl HlsAbrGroupKind {
    fn from_adapter(kind: AdapterAbrGroupKind) -> Self {
        match kind {
            AdapterAbrGroupKind::DashVideo => Self::DashVideo,
            AdapterAbrGroupKind::DashAudioOnly => Self::DashAudioOnly,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsVariantMetadata {
    pub(crate) id: String,
    pub(crate) kind: BilibiliPlaybackVariantKind,
    pub(crate) content_id: String,
    pub(crate) bandwidth: Option<u64>,
    pub(crate) codecs: Vec<String>,
    pub(crate) mime_types: Vec<String>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) frame_rate: Option<String>,
    pub(crate) duration_seconds: Option<u32>,
    pub(crate) abr: Option<HlsAbrLevel>,
    pub(crate) media: Vec<HlsMediaResourceMetadata>,
}

impl HlsVariantMetadata {
    fn from_adapter(variant: &AdapterPlaybackVariant) -> Self {
        let mut media = Vec::new();
        if let Some(video) = &variant.video {
            media.push(HlsMediaResourceMetadata::from_request(video));
        }
        if let Some(audio) = &variant.audio {
            media.push(HlsMediaResourceMetadata::from_request(audio));
        }
        media.extend(
            variant
                .flv_segments
                .iter()
                .map(HlsMediaResourceMetadata::from_request),
        );

        Self {
            id: variant.id.clone(),
            kind: variant.kind,
            content_id: variant.content_id.clone(),
            bandwidth: variant.bandwidth,
            codecs: variant.codecs.clone(),
            mime_types: variant.mime_types.clone(),
            width: variant.width,
            height: variant.height,
            frame_rate: variant.frame_rate.clone(),
            duration_seconds: variant.duration_seconds,
            abr: variant.abr.as_ref().map(HlsAbrLevel::from_adapter),
            media,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsAbrLevel {
    pub(crate) group_id: String,
    pub(crate) level_index: u32,
    pub(crate) level_count: u32,
    pub(crate) switchable: bool,
}

impl HlsAbrLevel {
    fn from_adapter(level: &AdapterAbrLevel) -> Self {
        Self {
            group_id: level.group_id.clone(),
            level_index: level.level_index,
            level_count: level.level_count,
            switchable: level.switchable,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsMediaResourceMetadata {
    pub(crate) kind: BilibiliMediaRequestKind,
    pub(crate) stream_id: Option<u32>,
    pub(crate) mime_type: Option<String>,
    pub(crate) codecs: Option<String>,
    pub(crate) bandwidth: Option<u64>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) frame_rate: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) duration_seconds: Option<u32>,
    pub(crate) cache_key: BilibiliMediaCacheKey,
}

impl HlsMediaResourceMetadata {
    fn from_request(request: &BilibiliMediaRequest) -> Self {
        Self {
            kind: request.kind,
            stream_id: request.stream_id,
            mime_type: request.mime_type.clone(),
            codecs: request.codecs.clone(),
            bandwidth: request.bandwidth,
            width: request.width,
            height: request.height,
            frame_rate: request.frame_rate.clone(),
            size: request.size,
            duration_seconds: request.duration_seconds,
            cache_key: request.cache_key.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsMediaResource {
    pub(crate) id: String,
    pub(crate) request: BilibiliMediaRequest,
}

impl HlsMediaResource {
    pub(crate) fn content_type(&self) -> &str {
        self.request
            .mime_type
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("application/octet-stream")
    }
}

pub(crate) fn should_forward_media_request_header(
    header_name: &str,
    primary_url: &str,
    target_url: &str,
) -> bool {
    if media_request_origins_match(primary_url, target_url) {
        return true;
    }

    is_cross_origin_safe_media_header(header_name)
}

fn media_request_origins_match(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (Url::parse(left), Url::parse(right)) else {
        return false;
    };

    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn is_cross_origin_safe_media_header(header_name: &str) -> bool {
    matches!(
        header_name.to_ascii_lowercase().as_str(),
        "accept" | "accept-language" | "origin" | "referer" | "referrer" | "user-agent"
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsSessionError {
    message: String,
}

impl HlsSessionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for HlsSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HlsSessionError {}

pub(crate) fn mp4_initialization_length(bytes: &[u8]) -> Option<u64> {
    let mut offset = 0_usize;
    while offset.checked_add(8)? <= bytes.len() {
        let size32 = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?);
        let box_type = &bytes[offset + 4..offset + 8];
        let (header_length, box_size) = match size32 {
            0 => return None,
            1 => {
                if offset.checked_add(16)? > bytes.len() {
                    return None;
                }
                (
                    16_u64,
                    u64::from_be_bytes(bytes[offset + 8..offset + 16].try_into().ok()?),
                )
            }
            size => (8_u64, u64::from(size)),
        };
        if box_size < header_length {
            return None;
        }
        let end = u64::try_from(offset).ok()?.checked_add(box_size)?;
        if end > u64::try_from(bytes.len()).ok()? {
            return None;
        }
        if box_type == b"moov" {
            return Some(end);
        }
        if matches!(box_type, b"moof" | b"mdat") {
            return None;
        }
        offset = usize::try_from(end).ok()?;
    }

    None
}

fn escape_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn push_unique_codec(codecs: &mut Vec<String>, codec: &str) {
    let codec = codec.trim();
    if codec.is_empty() || codecs.iter().any(|existing| existing == codec) {
        return;
    }
    codecs.push(codec.to_owned());
}

fn media_playlist_id(resource_id: &str) -> String {
    if let Some(stem) = resource_id.strip_suffix(".m4s") {
        return format!("{stem}.m3u8");
    }
    format!("{resource_id}.m3u8")
}

fn audio_group_id(index: usize) -> String {
    if index == 0 {
        "audio".to_owned()
    } else {
        format!("audio-v{index}")
    }
}

fn playable_alternate_variants(
    selected_variant: &AdapterPlaybackVariant,
    variants: &[AdapterPlaybackVariant],
) -> Result<Vec<HlsVariant>, HlsSessionError> {
    let mut alternates = Vec::new();
    for variant in variants {
        if variant.id == selected_variant.id
            || !is_switchable_alternate_variant(selected_variant, variant)
            || !has_matching_audio_presence(selected_variant, variant)
            || !is_avplayer_safe_alternate_variant(variant)
        {
            continue;
        }
        let index = alternates.len() + 1;
        alternates.push(HlsVariant::from_adapter_with_resource_ids(
            variant,
            format!("v{index}-video.m4s"),
            format!("v{index}-audio.m4s"),
        )?);
    }
    Ok(alternates)
}

fn hls_variant_codecs(
    variant: &AdapterPlaybackVariant,
    video: &BilibiliMediaRequest,
    audio: Option<&BilibiliMediaRequest>,
) -> Vec<String> {
    let mut codecs = Vec::new();
    if let Some(video_codecs) = video.codecs.as_deref() {
        push_unique_codec(&mut codecs, video_codecs);
    } else {
        for codec in &variant.codecs {
            push_unique_codec(&mut codecs, codec);
        }
    }
    if let Some(audio_codecs) = audio.and_then(|audio| audio.codecs.as_deref()) {
        push_unique_codec(&mut codecs, audio_codecs);
    }
    codecs
}

fn has_matching_audio_presence(
    selected_variant: &AdapterPlaybackVariant,
    variant: &AdapterPlaybackVariant,
) -> bool {
    selected_variant.audio.is_some() == variant.audio.is_some()
}

fn is_switchable_alternate_variant(
    selected_variant: &AdapterPlaybackVariant,
    variant: &AdapterPlaybackVariant,
) -> bool {
    let Some(selected_abr) = &selected_variant.abr else {
        return false;
    };
    let Some(variant_abr) = &variant.abr else {
        return false;
    };
    selected_abr.switchable
        && variant_abr.switchable
        && selected_abr.group_id == variant_abr.group_id
}

fn is_avplayer_safe_alternate_variant(variant: &AdapterPlaybackVariant) -> bool {
    if variant.kind != BilibiliPlaybackVariantKind::Dash {
        return false;
    }
    let Some(video) = &variant.video else {
        return false;
    };
    if !variant_has_codec(variant, video.codecs.as_deref(), is_h264_codec) {
        return false;
    }
    variant
        .audio
        .as_ref()
        .is_none_or(|audio| variant_has_codec(variant, audio.codecs.as_deref(), is_aac_codec))
}

fn variant_has_codec(
    variant: &AdapterPlaybackVariant,
    request_codecs: Option<&str>,
    predicate: fn(&str) -> bool,
) -> bool {
    if let Some(codecs) = request_codecs {
        return codec_list_matches(codecs, predicate);
    }
    variant
        .codecs
        .iter()
        .any(|codecs| codec_list_matches(codecs, predicate))
}

fn codec_list_matches(codecs: &str, predicate: fn(&str) -> bool) -> bool {
    codecs.split(',').any(predicate)
}

fn is_h264_codec(codec: &str) -> bool {
    let codec = codec.trim().to_ascii_lowercase();
    codec.starts_with("avc1") || codec.starts_with("avc3")
}

fn is_aac_codec(codec: &str) -> bool {
    codec.trim().to_ascii_lowercase().starts_with("mp4a")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bbdown_adapter::{
        BilibiliMediaCacheKey, BilibiliMediaRequestKind, BilibiliPlaybackVariant,
    };

    #[test]
    fn creates_master_and_media_playlists_for_dash_variant() {
        let session =
            HlsPlaybackSession::from_selected_variant("session-1", "Episode", &dash_variant())
                .unwrap();

        let master = session.master_playlist();
        assert!(master.contains("#EXT-X-STREAM-INF:BANDWIDTH=1000000"));
        assert!(master.contains("RESOLUTION=1920x1080"));
        assert!(master.contains("CODECS=\"avc1.640028,mp4a.40.2\""));
        assert!(master.contains("URI=\"segments/audio.m3u8\""));
        assert!(master.ends_with("segments/video.m3u8\n"));

        let video = session.media_playlist("video.m3u8", 128, 10_000).unwrap();
        assert!(video.contains("#EXT-X-TARGETDURATION:60"));
        assert!(video.contains("#EXT-X-MAP:URI=\"video.m4s\",BYTERANGE=\"128@0\""));
        assert!(video.contains("#EXT-X-BYTERANGE:9872@128"));
        assert!(video.contains("video.m4s"));

        let audio = session.media_playlist("audio.m3u8", 64, 10_000).unwrap();
        assert!(audio.contains("#EXT-X-MAP:URI=\"audio.m4s\",BYTERANGE=\"64@0\""));
        assert!(audio.contains("audio.m4s"));
    }

    #[test]
    fn master_playlist_deduplicates_audio_codec() {
        let mut variant = dash_variant();
        variant.codecs.push("mp4a.40.2".to_owned());
        let session =
            HlsPlaybackSession::from_selected_variant("session-1", "Episode", &variant).unwrap();

        let master = session.master_playlist();

        assert!(master.contains("CODECS=\"avc1.640028,mp4a.40.2\""));
        assert!(!master.contains("mp4a.40.2,mp4a.40.2"));
    }

    #[test]
    fn from_playback_entry_emits_avplayer_safe_alternate_hls_variants() {
        let mut selected = dash_variant();
        selected.abr = Some(abr_level("dash-video", 0, 2, true));
        let mut alternate = dash_variant();
        alternate.id = "h264-720p".to_owned();
        alternate.bandwidth = Some(600_000);
        alternate.codecs = vec!["hev1.1.6.L120.90".to_owned(), "mp4a.40.2".to_owned()];
        alternate.width = Some(1280);
        alternate.height = Some(720);
        alternate.abr = Some(abr_level("dash-video", 1, 2, true));
        alternate.video.as_mut().unwrap().url =
            "https://media.example.test/720p-video.m4s".to_owned();
        alternate.video.as_mut().unwrap().cache_key.source_hash =
            "h264-720p-video-source".to_owned();
        alternate.audio.as_mut().unwrap().url =
            "https://media.example.test/720p-audio.m4s".to_owned();
        alternate.audio.as_mut().unwrap().cache_key.source_hash =
            "h264-720p-audio-source".to_owned();

        let session = HlsPlaybackSession::from_playback_entry(
            "session-1",
            "Episode",
            &selected,
            &AdapterAbrMetadata { groups: Vec::new() },
            &[selected.clone(), alternate],
        )
        .unwrap();

        assert_eq!(1, session.alternate_variants.len());
        assert_eq!(
            vec!["avc1.640028".to_owned(), "mp4a.40.2".to_owned()],
            session.alternate_variants[0].codecs
        );
        let master = session.master_playlist();
        assert_eq!(2, master.matches("#EXT-X-STREAM-INF").count());
        assert!(master.contains("URI=\"segments/audio.m3u8\""));
        assert!(master.contains("URI=\"segments/v1-audio.m3u8\""));
        assert!(master.contains("AUDIO=\"audio-v1\""));
        assert!(master.contains("RESOLUTION=1280x720"));
        assert!(master.contains("segments/video.m3u8\n"));
        assert!(master.contains("segments/v1-video.m3u8\n"));
        assert!(!master.contains("hev1.1.6.L120.90"));

        let video = session
            .media_playlist("v1-video.m3u8", 128, 10_000)
            .unwrap();
        assert!(video.contains("#EXT-X-MAP:URI=\"v1-video.m4s\",BYTERANGE=\"128@0\""));
        assert!(video.contains("v1-video.m4s"));
        let audio = session.media_playlist_resource("v1-audio.m3u8").unwrap();
        assert_eq!("v1-audio.m4s", audio.id);
        let resource = session.media_resource("v1-video.m4s").unwrap();
        assert_eq!(
            "https://media.example.test/720p-video.m4s",
            resource.request.url
        );
    }

    #[test]
    fn from_playback_entry_filters_unsafe_alternate_hls_variants() {
        let mut selected = dash_variant();
        selected.abr = Some(abr_level("dash-video", 0, 6, true));
        let mut hevc = dash_variant();
        hevc.id = "hevc-1080p".to_owned();
        hevc.codecs = vec!["hev1.1.6.L120.90".to_owned()];
        hevc.abr = Some(abr_level("dash-video", 1, 6, true));
        hevc.video.as_mut().unwrap().codecs = Some("hev1.1.6.L120.90".to_owned());
        let mut av1 = dash_variant();
        av1.id = "av1-1080p".to_owned();
        av1.codecs = vec!["av01.0.08M.08".to_owned()];
        av1.abr = Some(abr_level("dash-video", 2, 6, true));
        av1.video.as_mut().unwrap().codecs = Some("av01.0.08M.08".to_owned());
        let mut mismatched_video = dash_variant();
        mismatched_video.id = "mismatched-video-codec".to_owned();
        mismatched_video.codecs = vec!["avc1.640028".to_owned(), "mp4a.40.2".to_owned()];
        mismatched_video.abr = Some(abr_level("dash-video", 3, 6, true));
        mismatched_video.video.as_mut().unwrap().codecs = Some("hev1.1.6.L120.90".to_owned());
        let mut mismatched_audio = dash_variant();
        mismatched_audio.id = "mismatched-audio-codec".to_owned();
        mismatched_audio.codecs = vec!["avc1.640028".to_owned(), "mp4a.40.2".to_owned()];
        mismatched_audio.abr = Some(abr_level("dash-video", 4, 6, true));
        mismatched_audio.audio.as_mut().unwrap().codecs = Some("flac".to_owned());
        let mut flv = dash_variant();
        flv.id = "flv".to_owned();
        flv.kind = BilibiliPlaybackVariantKind::Flv;
        flv.abr = Some(abr_level("dash-video", 5, 6, true));

        let session = HlsPlaybackSession::from_playback_entry(
            "session-1",
            "Episode",
            &selected,
            &AdapterAbrMetadata { groups: Vec::new() },
            &[
                selected.clone(),
                hevc,
                av1,
                mismatched_video,
                mismatched_audio,
                flv,
            ],
        )
        .unwrap();

        assert!(session.alternate_variants.is_empty());
        let master = session.master_playlist();
        assert_eq!(1, master.matches("#EXT-X-STREAM-INF").count());
        assert!(!master.contains("v1-video"));
    }

    #[test]
    fn from_playback_entry_filters_non_switchable_alternate_hls_variants() {
        let mut selected = dash_variant();
        selected.abr = Some(abr_level("dash-video", 0, 4, true));
        let mut non_switchable = dash_variant();
        non_switchable.id = "non-switchable".to_owned();
        non_switchable.abr = Some(abr_level("dash-video", 1, 4, false));
        let mut other_group = dash_variant();
        other_group.id = "other-group".to_owned();
        other_group.abr = Some(abr_level("dash-backup-video", 2, 4, true));
        let mut missing_abr = dash_variant();
        missing_abr.id = "missing-abr".to_owned();
        missing_abr.abr = None;

        let session = HlsPlaybackSession::from_playback_entry(
            "session-1",
            "Episode",
            &selected,
            &AdapterAbrMetadata { groups: Vec::new() },
            &[selected.clone(), non_switchable, other_group, missing_abr],
        )
        .unwrap();

        assert!(session.alternate_variants.is_empty());
        let master = session.master_playlist();
        assert_eq!(1, master.matches("#EXT-X-STREAM-INF").count());
        assert!(!master.contains("v1-video"));
    }

    #[test]
    fn from_playback_entry_filters_audio_incompatible_alternate_hls_variants() {
        let mut selected = dash_variant();
        selected.abr = Some(abr_level("dash-video", 0, 2, true));
        let mut audio_less = dash_variant();
        audio_less.id = "audio-less".to_owned();
        audio_less.abr = Some(abr_level("dash-video", 1, 2, true));
        audio_less.audio = None;

        let session = HlsPlaybackSession::from_playback_entry(
            "session-1",
            "Episode",
            &selected,
            &AdapterAbrMetadata { groups: Vec::new() },
            &[selected.clone(), audio_less],
        )
        .unwrap();

        assert!(session.alternate_variants.is_empty());
        let master = session.master_playlist();
        assert_eq!(1, master.matches("#EXT-X-STREAM-INF").count());
        assert!(!master.contains("v1-video"));
    }

    #[test]
    fn rejects_non_dash_variant() {
        let mut variant = dash_variant();
        variant.kind = BilibiliPlaybackVariantKind::Flv;

        let error = HlsPlaybackSession::from_selected_variant("session-1", "Episode", &variant)
            .unwrap_err();

        assert_eq!(
            "Progressive HLS playback requires a DASH playback variant.",
            error.to_string()
        );
    }

    #[test]
    fn filters_sensitive_media_request_headers_for_cross_origin_backups() {
        assert!(should_forward_media_request_header(
            "authorization",
            "https://cdn-a.example.test/video.m4s",
            "https://cdn-a.example.test/video.m4s"
        ));
        assert!(!should_forward_media_request_header(
            "authorization",
            "https://cdn-a.example.test/video.m4s",
            "https://cdn-b.example.test/video.m4s"
        ));
        assert!(!should_forward_media_request_header(
            "cookie",
            "https://cdn-a.example.test/video.m4s",
            "https://cdn-b.example.test/video.m4s"
        ));
        assert!(should_forward_media_request_header(
            "referer",
            "https://cdn-a.example.test/video.m4s",
            "https://cdn-b.example.test/video.m4s"
        ));
    }

    fn dash_variant() -> BilibiliPlaybackVariant {
        BilibiliPlaybackVariant {
            id: "h264".to_owned(),
            kind: BilibiliPlaybackVariantKind::Dash,
            content_id: "content-1".to_owned(),
            bandwidth: Some(1_000_000),
            codecs: vec!["avc1.640028".to_owned()],
            mime_types: vec!["video/mp4".to_owned()],
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some("60".to_owned()),
            duration_seconds: Some(60),
            abr: None,
            video: Some(media_request(
                BilibiliMediaRequestKind::Video,
                "https://media.example.test/video.m4s",
                "video/mp4",
                "avc1.640028",
            )),
            audio: Some(media_request(
                BilibiliMediaRequestKind::Audio,
                "https://media.example.test/audio.m4s",
                "audio/mp4",
                "mp4a.40.2",
            )),
            flv_segments: Vec::new(),
        }
    }

    fn abr_level(
        group_id: &str,
        level_index: u32,
        level_count: u32,
        switchable: bool,
    ) -> AdapterAbrLevel {
        AdapterAbrLevel {
            group_id: group_id.to_owned(),
            level_index,
            level_count,
            switchable,
        }
    }

    fn media_request(
        kind: BilibiliMediaRequestKind,
        url: &str,
        mime_type: &str,
        codecs: &str,
    ) -> BilibiliMediaRequest {
        BilibiliMediaRequest {
            kind,
            stream_id: None,
            url: url.to_owned(),
            backup_urls: Vec::new(),
            headers: Vec::new(),
            mime_type: Some(mime_type.to_owned()),
            codecs: Some(codecs.to_owned()),
            bandwidth: None,
            width: None,
            height: None,
            frame_rate: None,
            size: Some(10_000),
            duration_seconds: Some(60),
            cache_key: BilibiliMediaCacheKey {
                content_id: "content-1".to_owned(),
                media_kind: kind,
                stream_id: None,
                codecs: Some(codecs.to_owned()),
                source_hash: "source-hash".to_owned(),
            },
        }
    }
}
