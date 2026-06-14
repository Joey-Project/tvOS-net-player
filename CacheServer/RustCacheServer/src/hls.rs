use std::{
    collections::HashMap,
    fmt::{self, Display},
    sync::{Arc, RwLock},
};

use crate::bbdown_adapter::{
    BilibiliMediaRequest, BilibiliPlaybackVariant as AdapterPlaybackVariant,
    BilibiliPlaybackVariantKind,
};

const VIDEO_PLAYLIST_ID: &str = "video.m3u8";
const AUDIO_PLAYLIST_ID: &str = "audio.m3u8";
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

        let Some(video) = variant.video.clone() else {
            return Err(HlsSessionError::new(
                "Progressive HLS playback requires a video media request.",
            ));
        };

        Ok(Self {
            id: session_id.to_owned(),
            title: title.to_owned(),
            variant: HlsVariant {
                id: variant.id.clone(),
                bandwidth: variant
                    .bandwidth
                    .or(video.bandwidth)
                    .unwrap_or(DEFAULT_BANDWIDTH),
                codecs: variant.codecs.clone(),
                width: variant.width,
                height: variant.height,
                duration_seconds: variant
                    .duration_seconds
                    .or(video.duration_seconds)
                    .unwrap_or(DEFAULT_DURATION_SECONDS),
                video: HlsMediaResource {
                    id: VIDEO_SEGMENT_ID.to_owned(),
                    request: video,
                },
                audio: variant.audio.clone().map(|audio| HlsMediaResource {
                    id: AUDIO_SEGMENT_ID.to_owned(),
                    request: audio,
                }),
            },
        })
    }

    pub(crate) fn master_playlist(&self) -> String {
        let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
        if self.variant.audio.is_some() {
            playlist.push_str(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Default\",DEFAULT=YES,AUTOSELECT=YES,URI=\"segments/audio.m3u8\"\n",
            );
        }

        playlist.push_str("#EXT-X-STREAM-INF:");
        playlist.push_str(&format!("BANDWIDTH={}", self.variant.bandwidth));
        if let Some(resolution) = self.variant.resolution() {
            playlist.push_str(&format!(",RESOLUTION={resolution}"));
        }
        if let Some(codecs) = self.variant.codecs_attribute() {
            playlist.push_str(&format!(",CODECS=\"{}\"", escape_quoted(&codecs)));
        }
        if self.variant.audio.is_some() {
            playlist.push_str(",AUDIO=\"audio\"");
        }
        playlist.push_str("\nsegments/video.m3u8\n");
        playlist
    }

    pub(crate) fn media_playlist_resource(&self, playlist_id: &str) -> Option<HlsMediaResource> {
        match playlist_id {
            VIDEO_PLAYLIST_ID => Some(self.variant.video.clone()),
            AUDIO_PLAYLIST_ID => self.variant.audio.clone(),
            _ => None,
        }
    }

    pub(crate) fn media_playlist(
        &self,
        playlist_id: &str,
        initialization_length: u64,
        total_length: u64,
    ) -> Option<String> {
        let resource = match playlist_id {
            VIDEO_PLAYLIST_ID => &self.variant.video,
            AUDIO_PLAYLIST_ID => self.variant.audio.as_ref()?,
            _ => return None,
        };
        let media_length = total_length.checked_sub(initialization_length)?;
        if initialization_length == 0 || media_length == 0 {
            return None;
        }
        let duration = resource
            .request
            .duration_seconds
            .unwrap_or(self.variant.duration_seconds)
            .max(DEFAULT_DURATION_SECONDS);

        Some(format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:{duration}\n#EXT-X-MAP:URI=\"{}\",BYTERANGE=\"{initialization_length}@0\"\n#EXTINF:{duration}.000,\n#EXT-X-BYTERANGE:{media_length}@{initialization_length}\n{}\n#EXT-X-ENDLIST\n",
            resource.id, resource.id
        ))
    }

    pub(crate) fn media_resource(&self, segment_id: &str) -> Option<HlsMediaResource> {
        if segment_id == self.variant.video.id {
            return Some(self.variant.video.clone());
        }

        self.variant
            .audio
            .as_ref()
            .filter(|audio| segment_id == audio.id)
            .cloned()
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
