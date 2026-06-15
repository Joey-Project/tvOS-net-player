use std::{
    collections::HashSet,
    fs,
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
    hls::{
        HlsMediaResource, HlsPlaybackSession, HlsVariant, mp4_initialization_length,
        should_forward_media_request_header,
    },
    library::{OpenedMediaFile, open_read_no_follow},
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
        self.ensure_cache_directory(&session_dir)?;
        self.write_json_atomically(
            &session_dir.join("session.json"),
            &PersistedHlsSession::from(session.clone()),
        )
    }

    pub(crate) fn save_completed_session(&self, session: &HlsPlaybackSession) -> io::Result<()> {
        self.save_session(&sanitized_completed_session(session))
    }

    pub(crate) fn remove_session(&self, session_id: &str) -> io::Result<()> {
        let session_dir = self.session_dir(session_id)?;
        self.reject_cache_path_symlink(&session_dir)?;
        match fs::remove_dir_all(session_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn load_sessions(&self) -> io::Result<Vec<HlsPlaybackSession>> {
        let store_root = self.store_root();
        self.reject_cache_path_symlink(&store_root)?;
        let entries = match fs::read_dir(store_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.root_path.is_dir() => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };

        let mut sessions = Vec::new();
        for entry in entries.flatten() {
            let session_dir = entry.path();
            let Some(directory_session_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_cache_id(&directory_session_id).is_err() {
                continue;
            }
            if self.reject_cache_path_symlink(&session_dir).is_err() {
                continue;
            }
            let path = session_dir.join("session.json");
            let Some(bytes) = self.read_cache_file(&path) else {
                continue;
            };
            let Ok(persisted) = serde_json::from_slice::<PersistedHlsSession>(&bytes) else {
                continue;
            };
            if persisted.schema_version != HLS_CACHE_SCHEMA_VERSION {
                continue;
            }
            if persisted.id != directory_session_id {
                continue;
            }
            if let Ok(session) = HlsPlaybackSession::try_from(persisted) {
                sessions.push(session);
            }
        }

        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(sessions)
    }

    pub(crate) fn completed_session_ids(&self, sessions: &[HlsPlaybackSession]) -> HashSet<String> {
        sessions
            .iter()
            .filter_map(|session| {
                self.completed_library_item(session)
                    .map(|_| session.id.clone())
            })
            .collect()
    }

    pub(crate) fn list_completed_library_items(&self) -> Vec<LibraryItem> {
        let sessions = match self.load_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                eprintln!("Failed to scan completed HLS cache sessions: {error}");
                return Vec::new();
            }
        };
        let mut items = sessions
            .iter()
            .filter_map(|session| self.completed_library_item(session))
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

    pub(crate) fn completed_session(&self, session_id: &str) -> Option<HlsPlaybackSession> {
        let session = self.load_session(session_id)?;
        self.session_is_complete(&session).then_some(session)
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
        if !self.resource_cache_key_matches(session_id, resource_id, &metadata) {
            return None;
        }
        let file_path = self.resource_path(session_id, resource_id).ok()?;
        self.reject_cache_path_symlink(&file_path).ok()?;
        let file_metadata = fs::symlink_metadata(&file_path).ok()?;
        if file_metadata.file_type().is_symlink()
            || !file_metadata.is_file()
            || file_metadata.len() != metadata.total_length
        {
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
        let relative_path = self.resource_relative_path(session_id, resource_id).ok()?;
        let file = open_read_no_follow(self.root_path.as_ref(), &relative_path).ok()?;
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
            let _ = self.remove_session(&session.id);
            return Err(HlsCacheError::Cancelled);
        }
        let result = async {
            self.save_session(session)?;
            self.cache_resource(client, &session.id, &session.variant.video, &should_cancel)
                .await?;
            if let Some(audio) = &session.variant.audio {
                self.cache_resource(client, &session.id, audio, &should_cancel)
                    .await?;
            }
            self.save_completed_session(session)?;

            Ok(Self::completed_library_item_id(&session.id))
        }
        .await;
        if matches!(&result, Err(HlsCacheError::Cancelled)) {
            let _ = self.remove_session(&session.id);
        }
        result
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
        let bytes = self.read_cache_file(&path)?;
        let persisted = serde_json::from_slice::<PersistedHlsSession>(&bytes).ok()?;
        if persisted.schema_version != HLS_CACHE_SCHEMA_VERSION {
            return None;
        }
        if persisted.id != session_id {
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
        self.ensure_cache_directory(&session_dir)?;
        let resource_path = self.resource_path(session_id, &resource.id)?;
        let temp_path = resource_path.with_extension("tmp");
        let mut last_error = None;
        for url in resource_urls(resource) {
            if should_cancel() {
                return Err(HlsCacheError::Cancelled);
            }
            self.prepare_temp_path(&temp_path)?;
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
                    if should_cancel() {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return Err(HlsCacheError::Cancelled);
                    }
                    if let Err(error) = self.reject_cache_path_symlink(&resource_path) {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return Err(error.into());
                    }
                    tokio::fs::rename(&temp_path, &resource_path).await?;
                    if should_cancel() {
                        return Err(HlsCacheError::Cancelled);
                    }
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
                    self.write_json_atomically(
                        &self.resource_metadata_path(session_id, &resource.id)?,
                        &metadata,
                    )?;
                    if should_cancel() {
                        return Err(HlsCacheError::Cancelled);
                    }
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
        let bytes =
            self.read_cache_file(&self.resource_metadata_path(session_id, resource_id).ok()?)?;
        let metadata = serde_json::from_slice::<PersistedHlsCachedResource>(&bytes).ok()?;
        (metadata.schema_version == HLS_CACHE_SCHEMA_VERSION && metadata.id == resource_id)
            .then_some(metadata)
    }

    fn resource_cache_key_matches(
        &self,
        session_id: &str,
        resource_id: &str,
        metadata: &PersistedHlsCachedResource,
    ) -> bool {
        let Some(session) = self.load_session(session_id) else {
            return false;
        };
        let Some(resource) = session.media_resource(resource_id) else {
            return false;
        };
        metadata.cache_key
            == PersistedBilibiliMediaCacheKey::from(resource.request.cache_key.clone())
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

    fn resource_relative_path(&self, session_id: &str, resource_id: &str) -> io::Result<String> {
        validate_cache_id(session_id)?;
        validate_cache_id(resource_id)?;
        Ok(format!("{HLS_CACHE_DIR}/{session_id}/{resource_id}"))
    }

    fn resource_metadata_path(&self, session_id: &str, resource_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self
            .session_dir(session_id)?
            .join(format!("{resource_id}.json")))
    }

    fn ensure_cache_directory(&self, path: &Path) -> io::Result<()> {
        self.reject_cache_path_symlink(path)?;
        fs::create_dir_all(path)?;
        self.reject_cache_path_symlink(path)?;
        let metadata = fs::metadata(path)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "HLS cache path is not a directory",
            ));
        }
        Ok(())
    }

    fn prepare_temp_path(&self, path: &Path) -> io::Result<()> {
        self.reject_cache_path_symlink(path)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HLS cache temp path must not be a symlink",
            )),
            Ok(metadata) if metadata.is_file() => fs::remove_file(path),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "HLS cache temp path already exists and is not a file",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn write_json_atomically<T: Serialize>(&self, path: &Path, value: &T) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            self.ensure_cache_directory(parent)?;
        }
        let temp_path = path.with_extension("tmp");
        self.prepare_temp_path(&temp_path)?;
        let bytes = serde_json::to_vec_pretty(value).map_err(invalid_data)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = self.reject_cache_path_symlink(path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        fs::rename(temp_path, path)
    }

    fn reject_cache_path_symlink(&self, path: &Path) -> io::Result<()> {
        if cache_path_contains_symlink(self.root_path.as_ref(), path)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HLS cache path must not contain symlinks",
            ));
        }
        Ok(())
    }

    fn read_cache_file(&self, path: &Path) -> Option<Vec<u8>> {
        self.reject_cache_path_symlink(path).ok()?;
        fs::read(path).ok()
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
        if !should_forward_media_request_header(&header.name, &resource.request.url, url) {
            continue;
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
    let Some(maximum_length) = resource.request.size.or(declared_length) else {
        return Err(HlsCacheError::InvalidResource(
            "HLS resource length was unknown".to_owned(),
        ));
    };

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .await?;
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
        if total_length > maximum_length {
            return Err(HlsCacheError::InvalidResource(format!(
                "HLS resource body length exceeded expected size {maximum_length}"
            )));
        }
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

pub(crate) fn sanitized_completed_session(session: &HlsPlaybackSession) -> HlsPlaybackSession {
    let mut session = session.clone();
    sanitize_completed_resource(&mut session.variant.video);
    if let Some(audio) = session.variant.audio.as_mut() {
        sanitize_completed_resource(audio);
    }
    session
}

fn sanitize_completed_resource(resource: &mut HlsMediaResource) {
    resource.request.url.clear();
    resource.request.backup_urls.clear();
    resource.request.headers.clear();
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

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn cache_path_contains_symlink(root_path: &Path, candidate_path: &Path) -> io::Result<bool> {
    let root_path = absolute_path(root_path);
    let candidate_path = absolute_path(candidate_path);
    if !is_within_root(&root_path, &candidate_path) {
        return Ok(true);
    }

    if path_contains_symlink_component(&root_path)? {
        return Ok(true);
    }

    let Ok(relative_path) = candidate_path.strip_prefix(&root_path) else {
        return Ok(true);
    };
    let mut current_path = root_path;
    for component in relative_path.components() {
        current_path.push(component.as_os_str());
        match fs::symlink_metadata(&current_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }

    Ok(false)
}

fn path_contains_symlink_component(path: &Path) -> io::Result<bool> {
    let mut current_path = PathBuf::new();
    for component in absolute_path(path).components() {
        current_path.push(component.as_os_str());
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        if path_is_symlink(&current_path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn path_is_symlink(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn is_within_root(root_path: &Path, candidate_path: &Path) -> bool {
    candidate_path == root_path || candidate_path.starts_with(root_path)
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

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
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

    fn temp_store(temp: &TempDir) -> HlsCacheStore {
        HlsCacheStore::new(
            temp.path()
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(temp.path())),
        )
    }

    #[test]
    fn saves_and_loads_hls_session_manifest() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-1", "https://example.test/video.m4s");

        store
            .save_session(&session)
            .expect("session manifest should save");
        let sessions = store.load_sessions().expect("session manifest should load");

        assert_eq!(vec![session], sessions);
    }

    #[test]
    fn load_sessions_skips_manifest_with_mismatched_directory_id() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-1", "https://example.test/video.m4s");
        let mismatched_dir = temp
            .path()
            .join(".tvos-net-player")
            .join("hls")
            .join("orphan");
        std::fs::create_dir_all(&mismatched_dir).expect("session dir should be created");
        write_pretty_json(
            &mismatched_dir.join("session.json"),
            &PersistedHlsSession::from(session),
        );

        let sessions = store.load_sessions().expect("cache scan should succeed");

        assert!(sessions.is_empty());
    }

    #[test]
    fn get_completed_library_item_skips_manifest_with_mismatched_directory_id() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let completed_session = sample_session("session-2", "https://example.test/video.m4s");
        let completed_dir = store
            .session_dir(&completed_session.id)
            .expect("session dir should be valid");
        std::fs::create_dir_all(&completed_dir).expect("completed session dir should be created");
        write_pretty_json(
            &completed_dir.join("session.json"),
            &PersistedHlsSession::from(completed_session.clone()),
        );
        std::fs::write(
            store
                .resource_path(&completed_session.id, "video.m4s")
                .expect("resource path should be valid"),
            fake_mp4(),
        )
        .expect("completed resource should be written");
        write_pretty_json(
            &store
                .resource_metadata_path(&completed_session.id, "video.m4s")
                .expect("resource metadata path should be valid"),
            &cached_metadata_for_session(&completed_session, "video.m4s"),
        );
        let mismatched_dir = store
            .session_dir("orphan")
            .expect("mismatched session dir should be valid");
        std::fs::create_dir_all(&mismatched_dir).expect("mismatched session dir should be created");
        write_pretty_json(
            &mismatched_dir.join("session.json"),
            &PersistedHlsSession::from(completed_session),
        );

        assert!(
            store
                .get_completed_library_item("bilibili.hls.orphan")
                .is_none()
        );
    }

    #[test]
    fn load_sessions_reports_unreadable_store_path() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = temp.path().join(".tvos-net-player").join("hls");
        std::fs::create_dir_all(store_root.parent().unwrap()).unwrap();
        std::fs::write(&store_root, b"not a directory").unwrap();

        let error = store
            .load_sessions()
            .expect_err("non-directory store root should be reported");

        assert_eq!(io::ErrorKind::NotADirectory, error.kind());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hls_cache_root_symlink_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let real_parent = temp.path().join("real-parent");
        let real_root = real_parent.join("cache");
        std::fs::create_dir_all(&real_root).expect("real cache root should be created");
        let link_parent = temp.path().join("link-parent");
        symlink(&real_parent, &link_parent).expect("root ancestor symlink should be made");
        let store = HlsCacheStore::new(link_parent.join("cache"));
        let session = sample_session("session-1", "https://example.test/video.m4s");

        let save_error = store
            .save_session(&session)
            .expect_err("symlinked root ancestor should not be written");
        let load_error = store
            .load_sessions()
            .expect_err("symlinked root ancestor should not be scanned");

        assert_eq!(io::ErrorKind::PermissionDenied, save_error.kind());
        assert_eq!(io::ErrorKind::PermissionDenied, load_error.kind());
    }

    #[test]
    fn rejects_dot_segments_as_hls_cache_ids() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);

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
        let store = temp_store(&temp);
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
    async fn cached_resource_rejects_mismatched_request_cache_key() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-cache-key", &upstream_url);
        let client = reqwest::Client::new();
        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("session resources should cache");
        assert!(
            store
                .cached_resource("session-cache-key", "video.m4s")
                .is_some()
        );
        session.variant.video.request.cache_key.source_hash = "different-source".to_owned();
        store
            .save_completed_session(&session)
            .expect("tampered session manifest should save");

        assert!(
            store
                .cached_resource("session-cache-key", "video.m4s")
                .is_none()
        );
        assert!(store.get_completed_library_item(&item_id).is_none());
    }

    #[tokio::test]
    async fn rejects_short_hls_cache_response_with_declared_size() {
        let (upstream_url, _task) = start_short_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
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
    async fn rejects_lengthless_chunked_hls_cache_response() {
        let (upstream_url, _task) = start_overlong_chunked_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-lengthless", &upstream_url);
        let temp_path = store
            .resource_path("session-lengthless", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("lengthless response should be rejected");

        assert!(error.to_string().contains("length was unknown"));
        assert!(!temp_path.exists());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-lengthless")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_overlong_chunked_hls_cache_response_with_expected_size() {
        let (upstream_url, _task) = start_overlong_chunked_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-overlong", &upstream_url);
        session.variant.video.request.size = Some(fake_mp4().len() as u64);
        let temp_path = store
            .resource_path("session-overlong", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("overlong response should be rejected");

        assert!(error.to_string().contains("exceeded expected size"));
        assert!(!temp_path.exists());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-overlong")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_unsolicited_partial_hls_cache_response() {
        let (upstream_url, _task) = start_partial_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
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
        let store = temp_store(&temp);
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
        let store = temp_store(&temp);
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

    #[tokio::test]
    async fn does_not_forward_sensitive_headers_to_cross_origin_backup_url() {
        let (primary_url, _primary_task) = start_invalid_mp4_upstream().await;
        let (backup_url, _backup_task) = start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_mp4_reject_sensitive_headers)),
        )
        .await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-sensitive-backup", &primary_url);
        session.variant.video.request.backup_urls = vec![backup_url];
        session.variant.video.request.headers.extend([
            BilibiliHttpHeader {
                name: "authorization".to_owned(),
                value: "Bearer secret-token".to_owned(),
            },
            BilibiliHttpHeader {
                name: "cookie".to_owned(),
                value: "SESSDATA=secret-cookie".to_owned(),
            },
        ]);
        let client = reqwest::Client::new();

        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("cross-origin backup should not receive sensitive primary headers");
        let cached = store
            .cached_resource("session-sensitive-backup", "video.m4s")
            .expect("backup resource should be cached");

        assert_eq!("bilibili.hls.session-sensitive-backup", item_id);
        assert_eq!(fake_mp4().len() as u64, cached.total_length);
    }

    #[tokio::test]
    async fn completed_session_manifest_scrubs_upstream_request_data() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-scrubbed", &upstream_url);
        let backup_url = "https://cdn-backup.example.test/video.m4s".to_owned();
        session.variant.video.request.backup_urls = vec![backup_url.clone()];
        session.variant.video.request.headers.extend([
            BilibiliHttpHeader {
                name: "authorization".to_owned(),
                value: "Bearer secret-token".to_owned(),
            },
            BilibiliHttpHeader {
                name: "cookie".to_owned(),
                value: "SESSDATA=secret-cookie".to_owned(),
            },
        ]);
        let client = reqwest::Client::new();

        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("session resources should cache");
        let manifest_path = store
            .session_dir("session-scrubbed")
            .expect("session dir should be valid")
            .join("session.json");
        let manifest = std::fs::read_to_string(manifest_path)
            .expect("completed session manifest should remain readable");
        let sessions = store
            .load_sessions()
            .expect("completed session manifest should load");

        assert!(!manifest.contains(&upstream_url));
        assert!(!manifest.contains(&backup_url));
        assert!(!manifest.contains("secret-token"));
        assert!(!manifest.contains("SESSDATA"));
        assert_eq!(1, sessions.len());
        let request = &sessions[0].variant.video.request;
        assert!(request.url.is_empty());
        assert!(request.backup_urls.is_empty());
        assert!(request.headers.is_empty());
        assert!(store.get_completed_library_item(&item_id).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_cached_resource() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-symlink", "https://example.test/video.m4s");
        store
            .save_session(&session)
            .expect("session manifest should save");
        let target_path = temp.path().join("outside.mp4");
        std::fs::write(&target_path, fake_mp4()).expect("target file should be written");
        symlink(
            &target_path,
            store
                .resource_path("session-symlink", "video.m4s")
                .expect("resource path should be valid"),
        )
        .expect("resource symlink should be created");
        let metadata = PersistedHlsCachedResource {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: "video.m4s".to_owned(),
            content_type: session.variant.video.content_type().to_owned(),
            total_length: fake_mp4().len() as u64,
            initialization_length: 28,
            cache_key: PersistedBilibiliMediaCacheKey::from(
                session.variant.video.request.cache_key.clone(),
            ),
        };
        store
            .write_json_atomically(
                &store
                    .resource_metadata_path("session-symlink", "video.m4s")
                    .expect("metadata path should be valid"),
                &metadata,
            )
            .expect("resource metadata should save");

        assert!(
            store
                .cached_resource("session-symlink", "video.m4s")
                .is_none()
        );
        assert!(
            store
                .open_cached_resource("session-symlink", "video.m4s")
                .is_none()
        );
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-symlink")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_sessions_skips_symlinked_hls_cache_session_directory_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = temp.path().join(".tvos-net-player").join("hls");
        let outside_dir = temp.path().join("outside-session-read");
        let session = sample_session("session-read-link", "https://example.test/video.m4s");
        std::fs::create_dir_all(&store_root).expect("store root should be created");
        std::fs::create_dir(&outside_dir).expect("outside target should be created");
        write_pretty_json(
            &outside_dir.join("session.json"),
            &PersistedHlsSession::from(session.clone()),
        );
        symlink(&outside_dir, store_root.join(&session.id))
            .expect("session dir symlink should be made");

        let sessions = store.load_sessions().expect("cache scan should succeed");

        assert!(sessions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn load_session_rejects_symlinked_hls_cache_session_manifest_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-manifest-link", "https://example.test/video.m4s");
        let session_dir = store
            .session_dir(&session.id)
            .expect("session dir should be valid");
        std::fs::create_dir_all(&session_dir).expect("session dir should be created");
        let outside_manifest = temp.path().join("outside-session.json");
        write_pretty_json(
            &outside_manifest,
            &PersistedHlsSession::from(session.clone()),
        );
        symlink(&outside_manifest, session_dir.join("session.json"))
            .expect("session manifest symlink should be made");

        let sessions = store.load_sessions().expect("cache scan should succeed");

        assert!(sessions.is_empty());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-manifest-link")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_hls_cache_store_root_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let internal_dir = temp.path().join(".tvos-net-player");
        let outside_dir = temp.path().join("outside-hls-root");
        std::fs::create_dir_all(&internal_dir).expect("internal parent should be created");
        std::fs::create_dir(&outside_dir).expect("outside target should be created");
        symlink(&outside_dir, internal_dir.join("hls")).expect("store root symlink should be made");

        let error = store
            .load_sessions()
            .expect_err("symlinked HLS store root should be rejected");

        assert_eq!(io::ErrorKind::PermissionDenied, error.kind());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_hls_cache_metadata_file_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-metadata-link", "https://example.test/video.m4s");
        store
            .save_session(&session)
            .expect("session manifest should save");
        std::fs::write(
            store
                .resource_path(&session.id, "video.m4s")
                .expect("resource path should be valid"),
            fake_mp4(),
        )
        .expect("resource should be written");
        let outside_metadata = temp.path().join("outside-video.m4s.json");
        write_pretty_json(
            &outside_metadata,
            &cached_metadata_for_session(&session, "video.m4s"),
        );
        symlink(
            &outside_metadata,
            store
                .resource_metadata_path(&session.id, "video.m4s")
                .expect("metadata path should be valid"),
        )
        .expect("metadata symlink should be made");

        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-metadata-link")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_resource_rejects_symlinked_hls_cache_session_directory_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = temp.path().join(".tvos-net-player").join("hls");
        let outside_dir = temp.path().join("outside-session-resource");
        let session = sample_session("session-resource-link", "https://example.test/video.m4s");
        std::fs::create_dir_all(&store_root).expect("store root should be created");
        std::fs::create_dir(&outside_dir).expect("outside target should be created");
        std::fs::write(outside_dir.join("video.m4s"), fake_mp4())
            .expect("outside resource should be written");
        write_pretty_json(
            &outside_dir.join("video.m4s.json"),
            &cached_metadata_for_session(&session, "video.m4s"),
        );
        symlink(&outside_dir, store_root.join(&session.id))
            .expect("session dir symlink should be made");

        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_hls_cache_session_directory_for_writes_and_removal() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = temp.path().join(".tvos-net-player").join("hls");
        let outside_dir = temp.path().join("outside-session");
        std::fs::create_dir_all(&store_root).expect("store root should be created");
        std::fs::create_dir(&outside_dir).expect("outside target should be created");
        std::fs::write(outside_dir.join("keep.txt"), b"outside")
            .expect("outside sentinel should be written");
        symlink(&outside_dir, store_root.join("session-link"))
            .expect("session dir symlink should be made");
        let session = sample_session("session-link", "https://example.test/video.m4s");

        let save_error = store
            .save_session(&session)
            .expect_err("symlinked session dir should not be written");
        let remove_error = store
            .remove_session("session-link")
            .expect_err("symlinked session dir should not be removed");

        assert_eq!(io::ErrorKind::PermissionDenied, save_error.kind());
        assert_eq!(io::ErrorKind::PermissionDenied, remove_error.kind());
        assert_eq!(
            b"outside",
            std::fs::read(outside_dir.join("keep.txt"))
                .expect("outside sentinel should survive")
                .as_slice()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_hls_cache_temp_resource_path() {
        use std::os::unix::fs::symlink;

        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-temp-symlink", &upstream_url);
        store
            .save_session(&session)
            .expect("session manifest should save");
        let target_path = temp.path().join("outside-temp-target");
        std::fs::write(&target_path, b"outside").expect("target file should be written");
        let temp_path = store
            .resource_path("session-temp-symlink", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
        symlink(&target_path, &temp_path).expect("temp path symlink should be made");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("symlinked temp resource should be rejected");

        assert!(
            matches!(error, HlsCacheError::Io(error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
        assert_eq!(
            b"outside",
            std::fs::read(&target_path)
                .expect("target file should survive")
                .as_slice()
        );
        assert!(
            store
                .cached_resource("session-temp-symlink", "video.m4s")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_hls_cache_resource_path_before_commit() {
        use std::os::unix::fs::symlink;

        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-resource-symlink", &upstream_url);
        store
            .save_session(&session)
            .expect("session manifest should save");
        let target_path = temp.path().join("outside-resource-target");
        std::fs::write(&target_path, b"outside").expect("target file should be written");
        let resource_path = store
            .resource_path("session-resource-symlink", "video.m4s")
            .expect("resource path should be valid");
        symlink(&target_path, &resource_path).expect("resource path symlink should be made");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("symlinked resource target should be rejected before rename");

        assert!(
            matches!(error, HlsCacheError::Io(error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
        assert_eq!(
            b"outside",
            std::fs::read(&target_path)
                .expect("target file should survive")
                .as_slice()
        );
        assert!(
            std::fs::symlink_metadata(&resource_path)
                .expect("resource symlink should remain")
                .file_type()
                .is_symlink()
        );
        assert!(
            store
                .cached_resource("session-resource-symlink", "video.m4s")
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancellation_after_committed_resource_removes_partial_session() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session_with_audio("session-cancel-after-video", &upstream_url);
        let client = reqwest::Client::new();
        let store_for_cancel = store.clone();
        let session_id = session.id.clone();

        let error = store
            .cache_session_resources_until(&client, &session, move || {
                store_for_cancel
                    .cached_resource(&session_id, "video.m4s")
                    .is_some()
            })
            .await
            .expect_err("cancellation after video commit should stop finalization");

        assert!(matches!(error, HlsCacheError::Cancelled));
        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
        assert!(store.cached_resource(&session.id, "audio.m4s").is_none());
        assert!(
            store
                .get_completed_library_item(&format!("bilibili.hls.{}", session.id))
                .is_none()
        );
        assert!(
            !store
                .session_dir(&session.id)
                .expect("session dir should be valid")
                .exists()
        );
    }

    async fn start_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_mp4))).await
    }

    async fn start_short_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_short_mp4))).await
    }

    async fn start_overlong_chunked_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_overlong_chunked_mp4)),
        )
        .await
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

    async fn upstream_overlong_chunked_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = fake_mp4();
        let chunks = futures_util::stream::iter([
            Ok::<_, std::convert::Infallible>(body),
            Ok::<_, std::convert::Infallible>(b"extra".to_vec()),
        ]);
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .body(Body::from_stream(chunks))
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

    async fn upstream_mp4_reject_sensitive_headers(headers: HeaderMap) -> Response<Body> {
        if headers.contains_key("authorization") || headers.contains_key("cookie") {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap();
        }

        upstream_mp4(headers).await
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

    fn sample_session_with_audio(id: &str, url: &str) -> HlsPlaybackSession {
        let mut session = sample_session(id, url);
        let mut audio = session.variant.video.clone();
        audio.id = "audio.m4s".to_owned();
        audio.request.kind = BilibiliMediaRequestKind::Audio;
        audio.request.codecs = Some("mp4a.40.2".to_owned());
        audio.request.cache_key.media_kind = BilibiliMediaRequestKind::Audio;
        audio.request.cache_key.codecs = Some("mp4a.40.2".to_owned());
        session.variant.audio = Some(audio);
        session
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

    fn cached_metadata_for_session(
        session: &HlsPlaybackSession,
        resource_id: &str,
    ) -> PersistedHlsCachedResource {
        PersistedHlsCachedResource {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: resource_id.to_owned(),
            content_type: session.variant.video.content_type().to_owned(),
            total_length: fake_mp4().len() as u64,
            initialization_length: 28,
            cache_key: PersistedBilibiliMediaCacheKey::from(
                session.variant.video.request.cache_key.clone(),
            ),
        }
    }

    fn write_pretty_json<T: Serialize>(path: &Path, value: &T) {
        let mut bytes = serde_json::to_vec_pretty(value).expect("test JSON should serialize");
        bytes.push(b'\n');
        std::fs::write(path, bytes).expect("test JSON should be written");
    }
}
