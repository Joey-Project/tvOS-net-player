use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use futures_util::StreamExt;
use prost_types::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    bbdown_adapter::{
        BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest, BilibiliMediaRequestKind,
    },
    generated::tvos_net_player::v1::{
        LibraryItem, LibrarySource, MediaVariant, PlaybackProtocol, PlaybackSource,
    },
    hls::{HlsMediaResource, HlsPlaybackSession, HlsVariant, mp4_initialization_length},
    library::OpenedMediaFile,
};

const HLS_CACHE_SCHEMA_VERSION: u32 = 1;
const HLS_CACHE_DIR: &str = ".tvos-net-player/hls";
const HLS_LIBRARY_ITEM_PREFIX: &str = "bilibili.hls.";
const HLS_CACHE_VARIANT_LABEL: &str = "Offline HLS";
const HLS_INITIALIZATION_SCAN_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct HlsCacheStore {
    root_path: Arc<PathBuf>,
}

impl HlsCacheStore {
    pub(crate) fn new(root_path: impl Into<PathBuf>) -> Self {
        Self {
            root_path: Arc::new(root_path.into()),
        }
    }

    pub(crate) fn save_session(&self, session: &HlsPlaybackSession) -> io::Result<()> {
        let session_dir = self.session_dir(&session.id)?;
        fs::create_dir_all(&session_dir)?;
        write_json_atomically(
            &session_dir.join("session.json"),
            &PersistedHlsSession::from(session.clone()),
        )
    }

    pub(crate) fn remove_session(&self, session_id: &str) -> io::Result<()> {
        let session_dir = self.session_dir(session_id)?;
        match fs::remove_dir_all(session_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn load_sessions(&self) -> Vec<HlsPlaybackSession> {
        let Ok(entries) = fs::read_dir(self.store_root()) else {
            return Vec::new();
        };

        let mut sessions = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path().join("session.json");
            let Ok(bytes) = fs::read(path) else {
                continue;
            };
            let Ok(persisted) = serde_json::from_slice::<PersistedHlsSession>(&bytes) else {
                continue;
            };
            if persisted.schema_version != HLS_CACHE_SCHEMA_VERSION {
                continue;
            }
            if let Ok(session) = HlsPlaybackSession::try_from(persisted) {
                sessions.push(session);
            }
        }

        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        sessions
    }

    pub(crate) fn completed_session_ids(&self) -> HashSet<String> {
        self.load_sessions()
            .into_iter()
            .filter_map(|session| self.completed_library_item(&session).map(|_| session.id))
            .collect()
    }

    pub(crate) fn list_completed_library_items(&self) -> Vec<LibraryItem> {
        let mut items = self
            .load_sessions()
            .into_iter()
            .filter_map(|session| self.completed_library_item(&session))
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        items
    }

    pub(crate) fn get_completed_library_item(&self, item_id: &str) -> Option<LibraryItem> {
        let session_id = session_id_from_library_item_id(item_id)?;
        let session = self.load_session(&session_id)?;
        self.completed_library_item(&session)
    }

    pub(crate) fn create_playback_source(
        &self,
        item_id: &str,
        variant_id: &str,
        uri: String,
    ) -> Option<PlaybackSource> {
        let item = self.get_completed_library_item(item_id)?;
        if !item.variants.iter().any(|variant| variant.id == variant_id) {
            return None;
        }

        Some(PlaybackSource {
            item_id: item_id.to_owned(),
            variant_id: variant_id.to_owned(),
            protocol: PlaybackProtocol::Hls.into(),
            uri,
            expires_at: None,
        })
    }

    pub(crate) fn completed_library_item_id(session_id: &str) -> String {
        format!("{HLS_LIBRARY_ITEM_PREFIX}{session_id}")
    }

    pub(crate) fn session_id_from_library_item_id(item_id: &str) -> Option<String> {
        session_id_from_library_item_id(item_id)
    }

    pub(crate) fn cached_resource(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<CachedHlsResource> {
        let metadata = self.read_resource_metadata(session_id, resource_id)?;
        let file_path = self.resource_path(session_id, resource_id).ok()?;
        let file_metadata = fs::metadata(&file_path).ok()?;
        if !file_metadata.is_file() || file_metadata.len() != metadata.total_length {
            return None;
        }

        Some(CachedHlsResource {
            path: file_path,
            content_type: metadata.content_type,
            initialization_length: metadata.initialization_length,
            total_length: metadata.total_length,
            last_modified: file_metadata.modified().unwrap_or(UNIX_EPOCH),
        })
    }

    pub(crate) fn open_cached_resource(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<OpenedMediaFile> {
        let cached = self.cached_resource(session_id, resource_id)?;
        let file = File::open(&cached.path).ok()?;
        Some(OpenedMediaFile {
            file,
            content_type: cached.content_type,
            last_modified: cached.last_modified,
            size_bytes: cached.total_length,
        })
    }

    #[cfg(test)]
    pub(crate) async fn cache_session_resources(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
    ) -> Result<String, HlsCacheError> {
        self.cache_session_resources_until(client, session, || false)
            .await
    }

    pub(crate) async fn cache_session_resources_until<F>(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
        should_cancel: F,
    ) -> Result<String, HlsCacheError>
    where
        F: Fn() -> bool + Send + Sync,
    {
        if should_cancel() {
            return Err(HlsCacheError::Cancelled);
        }
        self.save_session(session)?;
        self.cache_resource(client, &session.id, &session.variant.video, &should_cancel)
            .await?;
        if let Some(audio) = &session.variant.audio {
            self.cache_resource(client, &session.id, audio, &should_cancel)
                .await?;
        }

        Ok(Self::completed_library_item_id(&session.id))
    }

    fn completed_library_item(&self, session: &HlsPlaybackSession) -> Option<LibraryItem> {
        if !self.session_is_complete(session) {
            return None;
        }

        let cached_video = self.cached_resource(&session.id, &session.variant.video.id)?;
        let size_bytes = self.cached_resource_total_size(session).unwrap_or_default();
        let updated_at = self
            .resource_modification_times(session)
            .into_iter()
            .max()
            .unwrap_or(UNIX_EPOCH);

        Some(LibraryItem {
            id: Self::completed_library_item_id(&session.id),
            title: session.title.clone(),
            subtitle: "Bilibili offline HLS cache".to_owned(),
            source: LibrarySource::Bilibili.into(),
            source_id: session.id.clone(),
            poster_uri: String::new(),
            variants: vec![MediaVariant {
                id: session.variant.id.clone(),
                label: HLS_CACHE_VARIANT_LABEL.to_owned(),
                protocol: PlaybackProtocol::Hls.into(),
                container: "hls".to_owned(),
                video_codec: session.variant.codecs.first().cloned().unwrap_or_default(),
                audio_codec: session
                    .variant
                    .audio
                    .as_ref()
                    .and_then(|audio| audio.request.codecs.clone())
                    .unwrap_or_default(),
                width: session
                    .variant
                    .width
                    .unwrap_or_default()
                    .try_into()
                    .unwrap_or(i32::MAX),
                height: session
                    .variant
                    .height
                    .unwrap_or_default()
                    .try_into()
                    .unwrap_or(i32::MAX),
                bitrate: session.variant.bandwidth.try_into().unwrap_or(i64::MAX),
                size_bytes: size_bytes.try_into().unwrap_or(i64::MAX),
            }],
            created_at: Some(timestamp_from_system_time(
                created_time_for_path(&cached_video.path).unwrap_or(UNIX_EPOCH),
            )),
            updated_at: Some(timestamp_from_system_time(updated_at)),
        })
    }

    fn session_is_complete(&self, session: &HlsPlaybackSession) -> bool {
        self.cached_resource(&session.id, &session.variant.video.id)
            .is_some()
            && session
                .variant
                .audio
                .as_ref()
                .is_none_or(|audio| self.cached_resource(&session.id, &audio.id).is_some())
    }

    fn cached_resource_total_size(&self, session: &HlsPlaybackSession) -> Option<u64> {
        let mut total = self
            .cached_resource(&session.id, &session.variant.video.id)?
            .total_length;
        if let Some(audio) = &session.variant.audio {
            total =
                total.checked_add(self.cached_resource(&session.id, &audio.id)?.total_length)?;
        }
        Some(total)
    }

    fn resource_modification_times(&self, session: &HlsPlaybackSession) -> Vec<SystemTime> {
        session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
            .filter_map(|resource| self.cached_resource(&session.id, &resource.id))
            .map(|resource| resource.last_modified)
            .collect()
    }

    fn load_session(&self, session_id: &str) -> Option<HlsPlaybackSession> {
        let path = self.session_dir(session_id).ok()?.join("session.json");
        let bytes = fs::read(path).ok()?;
        let persisted = serde_json::from_slice::<PersistedHlsSession>(&bytes).ok()?;
        if persisted.schema_version != HLS_CACHE_SCHEMA_VERSION {
            return None;
        }

        HlsPlaybackSession::try_from(persisted).ok()
    }

    async fn cache_resource(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        should_cancel: &(impl Fn() -> bool + Send + Sync),
    ) -> Result<(), HlsCacheError> {
        if should_cancel() {
            return Err(HlsCacheError::Cancelled);
        }
        if self.cached_resource(session_id, &resource.id).is_some() {
            return Ok(());
        }

        let session_dir = self.session_dir(session_id)?;
        tokio::fs::create_dir_all(&session_dir).await?;
        let resource_path = self.resource_path(session_id, &resource.id)?;
        let temp_path = resource_path.with_extension("tmp");
        let mut last_error = None;
        for url in resource_urls(resource) {
            if should_cancel() {
                return Err(HlsCacheError::Cancelled);
            }
            match download_resource(client, resource, &url, &temp_path, should_cancel).await {
                Ok(total_length) => {
                    let initialization_length =
                        match cached_mp4_initialization_length(&temp_path).await {
                            Ok(length) => length,
                            Err(error) => {
                                let _ = tokio::fs::remove_file(&temp_path).await;
                                last_error = Some(error);
                                continue;
                            }
                        };
                    if initialization_length == 0 || initialization_length >= total_length {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        last_error = Some(HlsCacheError::InvalidResource(
                            "cached HLS MP4 initialization range was invalid".to_owned(),
                        ));
                        continue;
                    }
                    tokio::fs::rename(&temp_path, &resource_path).await?;
                    let metadata = PersistedHlsCachedResource {
                        schema_version: HLS_CACHE_SCHEMA_VERSION,
                        id: resource.id.clone(),
                        content_type: resource.content_type().to_owned(),
                        total_length,
                        initialization_length,
                        cache_key: PersistedBilibiliMediaCacheKey::from(
                            resource.request.cache_key.clone(),
                        ),
                    };
                    write_json_atomically(
                        &self.resource_metadata_path(session_id, &resource.id)?,
                        &metadata,
                    )?;
                    return Ok(());
                }
                Err(error) => {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            HlsCacheError::InvalidResource("HLS media request did not contain a URL".to_owned())
        }))
    }

    fn read_resource_metadata(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<PersistedHlsCachedResource> {
        let bytes = fs::read(self.resource_metadata_path(session_id, resource_id).ok()?).ok()?;
        let metadata = serde_json::from_slice::<PersistedHlsCachedResource>(&bytes).ok()?;
        (metadata.schema_version == HLS_CACHE_SCHEMA_VERSION && metadata.id == resource_id)
            .then_some(metadata)
    }

    fn store_root(&self) -> PathBuf {
        self.root_path.join(HLS_CACHE_DIR)
    }

    fn session_dir(&self, session_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(session_id)?;
        Ok(self.store_root().join(session_id))
    }

    fn resource_path(&self, session_id: &str, resource_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self.session_dir(session_id)?.join(resource_id))
    }

    fn resource_metadata_path(&self, session_id: &str, resource_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self
            .session_dir(session_id)?
            .join(format!("{resource_id}.json")))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CachedHlsResource {
    pub(crate) path: PathBuf,
    pub(crate) content_type: String,
    pub(crate) initialization_length: u64,
    pub(crate) total_length: u64,
    pub(crate) last_modified: SystemTime,
}

#[derive(Debug)]
pub(crate) enum HlsCacheError {
    Io(io::Error),
    Network(reqwest::Error),
    UpstreamStatus(StatusCode),
    InvalidResource(String),
    Cancelled,
}

impl From<io::Error> for HlsCacheError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<reqwest::Error> for HlsCacheError {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(error)
    }
}

impl std::fmt::Display for HlsCacheError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Network(error) => write!(formatter, "network error: {error}"),
            Self::UpstreamStatus(status) => write!(formatter, "upstream returned {status}"),
            Self::InvalidResource(message) => formatter.write_str(message),
            Self::Cancelled => formatter.write_str("HLS cache finalization was cancelled"),
        }
    }
}

impl std::error::Error for HlsCacheError {}

async fn download_resource(
    client: &reqwest::Client,
    resource: &HlsMediaResource,
    url: &str,
    temp_path: &Path,
    should_cancel: &(impl Fn() -> bool + Send + Sync),
) -> Result<u64, HlsCacheError> {
    if should_cancel() {
        return Err(HlsCacheError::Cancelled);
    }
    let mut request = client.get(url);
    let mut requested_range = false;
    for header in &resource.request.headers {
        if header.name.eq_ignore_ascii_case("range") {
            requested_range = true;
        }
        request = request.header(header.name.as_str(), header.value.as_str());
    }
    if requested_range {
        return Err(HlsCacheError::InvalidResource(
            "offline HLS cache does not support range-only media requests".to_owned(),
        ));
    }
    let response = request.send().await?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        return Err(HlsCacheError::UpstreamStatus(status));
    }
    if status == StatusCode::PARTIAL_CONTENT {
        return Err(HlsCacheError::InvalidResource(
            "offline HLS cache received partial content for a full media resource".to_owned(),
        ));
    }
    let declared_length = response.content_length();
    if let (Some(expected), Some(declared)) = (resource.request.size, declared_length)
        && declared != expected
    {
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS resource Content-Length {declared} did not match expected size {expected}"
        )));
    }

    let mut file = tokio::fs::File::create(temp_path).await?;
    let mut stream = response.bytes_stream();
    let mut total_length = 0_u64;
    loop {
        if should_cancel() {
            return Err(HlsCacheError::Cancelled);
        }
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk?;
        if should_cancel() {
            return Err(HlsCacheError::Cancelled);
        }
        total_length = total_length
            .checked_add(chunk.len().try_into().unwrap_or(u64::MAX))
            .ok_or_else(|| {
                HlsCacheError::InvalidResource("HLS resource is too large".to_owned())
            })?;
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    if let Some(declared) = declared_length
        && total_length != declared
    {
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS resource body length {total_length} did not match Content-Length {declared}"
        )));
    }
    if let Some(expected) = resource.request.size
        && total_length != expected
    {
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS resource body length {total_length} did not match expected size {expected}"
        )));
    }

    Ok(total_length)
}

async fn cached_mp4_initialization_length(path: &Path) -> Result<u64, HlsCacheError> {
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(HLS_INITIALIZATION_SCAN_BYTES)
        .read_to_end(&mut bytes)
        .await?;
    mp4_initialization_length(&bytes).ok_or_else(|| {
        HlsCacheError::InvalidResource("cached HLS MP4 init box not found".to_owned())
    })
}

fn resource_urls(resource: &HlsMediaResource) -> Vec<String> {
    let mut urls = Vec::with_capacity(resource.request.backup_urls.len() + 1);
    if !resource.request.url.trim().is_empty() {
        urls.push(resource.request.url.clone());
    }
    urls.extend(resource.request.backup_urls.clone());
    urls
}

fn session_id_from_library_item_id(item_id: &str) -> Option<String> {
    item_id
        .strip_prefix(HLS_LIBRARY_ITEM_PREFIX)
        .map(str::to_owned)
        .filter(|session_id| validate_cache_id(session_id).is_ok())
}

fn validate_cache_id(value: &str) -> io::Result<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid HLS cache identifier",
        ));
    }

    Ok(())
}

fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(invalid_data)?;
    let mut file = File::create(&temp_path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp_path, path)
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn created_time_for_path(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.created().ok()
}

fn timestamp_from_system_time(time: SystemTime) -> Timestamp {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos().try_into().unwrap_or(i32::MAX),
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsSession {
    schema_version: u32,
    id: String,
    title: String,
    variant: PersistedHlsVariant,
}

impl From<HlsPlaybackSession> for PersistedHlsSession {
    fn from(session: HlsPlaybackSession) -> Self {
        Self {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: session.id,
            title: session.title,
            variant: PersistedHlsVariant::from(session.variant),
        }
    }
}

impl TryFrom<PersistedHlsSession> for HlsPlaybackSession {
    type Error = ();

    fn try_from(session: PersistedHlsSession) -> Result<Self, Self::Error> {
        validate_cache_id(&session.id).map_err(|_| ())?;
        Ok(Self {
            id: session.id,
            title: session.title,
            variant: HlsVariant::try_from(session.variant)?,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsVariant {
    id: String,
    bandwidth: u64,
    #[serde(default)]
    codecs: Vec<String>,
    width: Option<u32>,
    height: Option<u32>,
    duration_seconds: u32,
    video: PersistedHlsMediaResource,
    audio: Option<PersistedHlsMediaResource>,
}

impl From<HlsVariant> for PersistedHlsVariant {
    fn from(variant: HlsVariant) -> Self {
        Self {
            id: variant.id,
            bandwidth: variant.bandwidth,
            codecs: variant.codecs,
            width: variant.width,
            height: variant.height,
            duration_seconds: variant.duration_seconds,
            video: PersistedHlsMediaResource::from(variant.video),
            audio: variant.audio.map(PersistedHlsMediaResource::from),
        }
    }
}

impl TryFrom<PersistedHlsVariant> for HlsVariant {
    type Error = ();

    fn try_from(variant: PersistedHlsVariant) -> Result<Self, Self::Error> {
        validate_cache_id(&variant.id).map_err(|_| ())?;
        Ok(Self {
            id: variant.id,
            bandwidth: variant.bandwidth,
            codecs: variant.codecs,
            width: variant.width,
            height: variant.height,
            duration_seconds: variant.duration_seconds,
            video: HlsMediaResource::try_from(variant.video)?,
            audio: variant.audio.map(HlsMediaResource::try_from).transpose()?,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsMediaResource {
    id: String,
    request: PersistedBilibiliMediaRequest,
}

impl From<HlsMediaResource> for PersistedHlsMediaResource {
    fn from(resource: HlsMediaResource) -> Self {
        Self {
            id: resource.id,
            request: PersistedBilibiliMediaRequest::from(resource.request),
        }
    }
}

impl TryFrom<PersistedHlsMediaResource> for HlsMediaResource {
    type Error = ();

    fn try_from(resource: PersistedHlsMediaResource) -> Result<Self, Self::Error> {
        validate_cache_id(&resource.id).map_err(|_| ())?;
        Ok(Self {
            id: resource.id,
            request: BilibiliMediaRequest::from(resource.request),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsCachedResource {
    schema_version: u32,
    id: String,
    content_type: String,
    total_length: u64,
    initialization_length: u64,
    cache_key: PersistedBilibiliMediaCacheKey,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliMediaRequest {
    kind: PersistedBilibiliMediaRequestKind,
    stream_id: Option<u32>,
    url: String,
    #[serde(default)]
    backup_urls: Vec<String>,
    #[serde(default)]
    headers: Vec<PersistedBilibiliHttpHeader>,
    mime_type: Option<String>,
    codecs: Option<String>,
    bandwidth: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<String>,
    size: Option<u64>,
    duration_seconds: Option<u32>,
    cache_key: PersistedBilibiliMediaCacheKey,
}

impl From<BilibiliMediaRequest> for PersistedBilibiliMediaRequest {
    fn from(request: BilibiliMediaRequest) -> Self {
        Self {
            kind: PersistedBilibiliMediaRequestKind::from(request.kind),
            stream_id: request.stream_id,
            url: request.url,
            backup_urls: request.backup_urls,
            headers: request
                .headers
                .into_iter()
                .map(PersistedBilibiliHttpHeader::from)
                .collect(),
            mime_type: request.mime_type,
            codecs: request.codecs,
            bandwidth: request.bandwidth,
            width: request.width,
            height: request.height,
            frame_rate: request.frame_rate,
            size: request.size,
            duration_seconds: request.duration_seconds,
            cache_key: PersistedBilibiliMediaCacheKey::from(request.cache_key),
        }
    }
}

impl From<PersistedBilibiliMediaRequest> for BilibiliMediaRequest {
    fn from(request: PersistedBilibiliMediaRequest) -> Self {
        Self {
            kind: BilibiliMediaRequestKind::from(request.kind),
            stream_id: request.stream_id,
            url: request.url,
            backup_urls: request.backup_urls,
            headers: request
                .headers
                .into_iter()
                .map(BilibiliHttpHeader::from)
                .collect(),
            mime_type: request.mime_type,
            codecs: request.codecs,
            bandwidth: request.bandwidth,
            width: request.width,
            height: request.height,
            frame_rate: request.frame_rate,
            size: request.size,
            duration_seconds: request.duration_seconds,
            cache_key: BilibiliMediaCacheKey::from(request.cache_key),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliHttpHeader {
    name: String,
    value: String,
}

impl From<BilibiliHttpHeader> for PersistedBilibiliHttpHeader {
    fn from(header: BilibiliHttpHeader) -> Self {
        Self {
            name: header.name,
            value: header.value,
        }
    }
}

impl From<PersistedBilibiliHttpHeader> for BilibiliHttpHeader {
    fn from(header: PersistedBilibiliHttpHeader) -> Self {
        Self {
            name: header.name,
            value: header.value,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliMediaCacheKey {
    content_id: String,
    media_kind: PersistedBilibiliMediaRequestKind,
    stream_id: Option<u32>,
    codecs: Option<String>,
    source_hash: String,
}

impl From<BilibiliMediaCacheKey> for PersistedBilibiliMediaCacheKey {
    fn from(key: BilibiliMediaCacheKey) -> Self {
        Self {
            content_id: key.content_id,
            media_kind: PersistedBilibiliMediaRequestKind::from(key.media_kind),
            stream_id: key.stream_id,
            codecs: key.codecs,
            source_hash: key.source_hash,
        }
    }
}

impl From<PersistedBilibiliMediaCacheKey> for BilibiliMediaCacheKey {
    fn from(key: PersistedBilibiliMediaCacheKey) -> Self {
        Self {
            content_id: key.content_id,
            media_kind: BilibiliMediaRequestKind::from(key.media_kind),
            stream_id: key.stream_id,
            codecs: key.codecs,
            source_hash: key.source_hash,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedBilibiliMediaRequestKind {
    Video,
    Audio,
    FlvSegment,
}

impl From<BilibiliMediaRequestKind> for PersistedBilibiliMediaRequestKind {
    fn from(kind: BilibiliMediaRequestKind) -> Self {
        match kind {
            BilibiliMediaRequestKind::Video => Self::Video,
            BilibiliMediaRequestKind::Audio => Self::Audio,
            BilibiliMediaRequestKind::FlvSegment => Self::FlvSegment,
        }
    }
}

impl From<PersistedBilibiliMediaRequestKind> for BilibiliMediaRequestKind {
    fn from(kind: PersistedBilibiliMediaRequestKind) -> Self {
        match kind {
            PersistedBilibiliMediaRequestKind::Video => Self::Video,
            PersistedBilibiliMediaRequestKind::Audio => Self::Audio,
            PersistedBilibiliMediaRequestKind::FlvSegment => Self::FlvSegment,
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Body,
        http::{HeaderMap, HeaderValue, Response, header::CONTENT_LENGTH, header::CONTENT_TYPE},
        routing::get,
    };
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn saves_and_loads_hls_session_manifest() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = HlsCacheStore::new(temp.path());
        let session = sample_session("session-1", "https://example.test/video.m4s");

        store
            .save_session(&session)
            .expect("session manifest should save");
        let sessions = store.load_sessions();

        assert_eq!(vec![session], sessions);
    }

    #[test]
    fn rejects_dot_segments_as_hls_cache_ids() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = HlsCacheStore::new(temp.path());

        assert!(validate_cache_id(".").is_err());
        assert!(validate_cache_id("..").is_err());
        assert!(store.session_dir("..").is_err());
        assert!(
            store
                .get_completed_library_item("bilibili.hls...")
                .is_none()
        );
    }

    #[tokio::test]
    async fn caches_session_resources_and_exposes_completed_library_item() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = HlsCacheStore::new(temp.path());
        let session = sample_session("session-1", &upstream_url);
        let client = reqwest::Client::new();

        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("session resources should cache");
        let item = store
            .get_completed_library_item(&item_id)
            .expect("completed session should expose a library item");
        let cached = store
            .cached_resource("session-1", "video.m4s")
            .expect("resource should be cached");

        assert_eq!("bilibili.hls.session-1", item.id);
        assert_eq!("Episode", item.title);
        assert_eq!(PlaybackProtocol::Hls as i32, item.variants[0].protocol);
        assert_eq!(fake_mp4().len() as u64, cached.total_length);
        assert_eq!(28, cached.initialization_length);
    }

    #[tokio::test]
    async fn rejects_short_hls_cache_response_with_declared_size() {
        let (upstream_url, _task) = start_short_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = HlsCacheStore::new(temp.path());
        let mut session = sample_session("session-short", &upstream_url);
        session.variant.video.request.size = Some(fake_mp4().len() as u64);
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("short response should be rejected");

        assert!(error.to_string().contains("expected size"));
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-short")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_unsolicited_partial_hls_cache_response() {
        let (upstream_url, _task) = start_partial_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = HlsCacheStore::new(temp.path());
        let mut session = sample_session("session-partial", &upstream_url);
        session.variant.video.request.size = Some(fake_mp4().len() as u64);
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("partial response should be rejected");

        assert!(error.to_string().contains("partial content"));
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-partial")
                .is_none()
        );
    }

    #[tokio::test]
    async fn removes_temp_file_when_cached_initialization_is_invalid() {
        let (upstream_url, _task) = start_invalid_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = HlsCacheStore::new(temp.path());
        let mut session = sample_session("session-invalid", &upstream_url);
        session.variant.video.request.size = Some(invalid_mp4().len() as u64);
        let temp_path = store
            .resource_path("session-invalid", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("invalid MP4 should be rejected");

        assert!(error.to_string().contains("init box not found"));
        assert!(!temp_path.exists());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-invalid")
                .is_none()
        );
    }

    #[tokio::test]
    async fn tries_backup_url_after_cached_initialization_is_invalid() {
        let (primary_url, _primary_task) = start_invalid_mp4_upstream().await;
        let (backup_url, _backup_task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = HlsCacheStore::new(temp.path());
        let mut session = sample_session("session-backup", &primary_url);
        session.variant.video.request.backup_urls = vec![backup_url];
        let temp_path = store
            .resource_path("session-backup", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
        let client = reqwest::Client::new();

        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("backup URL should cache after invalid primary");
        let cached = store
            .cached_resource("session-backup", "video.m4s")
            .expect("backup resource should be cached");

        assert_eq!("bilibili.hls.session-backup", item_id);
        assert_eq!(fake_mp4().len() as u64, cached.total_length);
        assert_eq!(28, cached.initialization_length);
        assert!(!temp_path.exists());
    }

    async fn start_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_mp4))).await
    }

    async fn start_short_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_short_mp4))).await
    }

    async fn start_partial_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_partial_mp4))).await
    }

    async fn start_invalid_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_invalid_mp4))).await
    }

    async fn start_hls_cache_upstream(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("upstream should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn upstream_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .body(Body::from(fake_mp4()))
            .unwrap()
    }

    async fn upstream_short_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = fake_mp4();
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, (body.len() - 4).to_string())
            .body(Body::from(body[..body.len() - 4].to_vec()))
            .unwrap()
    }

    async fn upstream_partial_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = fake_mp4();
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, body.len().to_string())
            .body(Body::from(body))
            .unwrap()
    }

    async fn upstream_invalid_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = invalid_mp4();
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, body.len().to_string())
            .body(Body::from(body))
            .unwrap()
    }

    fn sample_session(id: &str, url: &str) -> HlsPlaybackSession {
        HlsPlaybackSession {
            id: id.to_owned(),
            title: "Episode".to_owned(),
            variant: HlsVariant {
                id: "h264".to_owned(),
                bandwidth: 1_000_000,
                codecs: vec!["avc1.640028".to_owned()],
                width: Some(1920),
                height: Some(1080),
                duration_seconds: 60,
                video: HlsMediaResource {
                    id: "video.m4s".to_owned(),
                    request: BilibiliMediaRequest {
                        kind: BilibiliMediaRequestKind::Video,
                        stream_id: None,
                        url: url.to_owned(),
                        backup_urls: Vec::new(),
                        headers: vec![BilibiliHttpHeader {
                            name: "referer".to_owned(),
                            value: "https://www.bilibili.com".to_owned(),
                        }],
                        mime_type: Some("video/mp4".to_owned()),
                        codecs: Some("avc1.640028".to_owned()),
                        bandwidth: Some(1_000_000),
                        width: Some(1920),
                        height: Some(1080),
                        frame_rate: Some("60".to_owned()),
                        size: None,
                        duration_seconds: Some(60),
                        cache_key: BilibiliMediaCacheKey {
                            content_id: "cid-1".to_owned(),
                            media_kind: BilibiliMediaRequestKind::Video,
                            stream_id: None,
                            codecs: Some("avc1.640028".to_owned()),
                            source_hash: "source-hash".to_owned(),
                        },
                    },
                },
                audio: None,
            },
        }
    }

    fn fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes.extend(mp4_box(*b"moof", b"frag"));
        bytes.extend(mp4_box(*b"mdat", b"media-data"));
        bytes
    }

    fn invalid_mp4() -> Vec<u8> {
        b"not-fragmented-mp4".to_vec()
    }

    fn mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).unwrap();
        let mut bytes = Vec::new();
        bytes.extend(size.to_be_bytes());
        bytes.extend(kind);
        bytes.extend(payload);
        bytes
    }
}
