use std::{
    cmp::Ordering,
    ffi::CString,
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use prost_types::Timestamp;

use crate::{
    config::CacheServerOptions,
    generated::tvos_net_player::v1::{
        CacheRoot, CacheRootKind, LibraryFilter, LibraryItem, LibrarySource, MediaVariant,
        PlaybackProtocol,
    },
};

pub const ROOT_ID: &str = "default";
pub const VARIANT_ID: &str = "original";

#[derive(Clone)]
pub struct LocalMediaLibrary {
    options: Arc<CacheServerOptions>,
}

impl LocalMediaLibrary {
    pub fn new(options: Arc<CacheServerOptions>) -> Self {
        Self { options }
    }

    pub fn root_path(&self) -> PathBuf {
        absolute_path(&self.options.root_path)
    }

    pub fn supports_http_range_playback(&self) -> bool {
        supports_secure_no_follow_open()
    }

    pub async fn list_items_page(
        &self,
        filter: Option<&LibraryFilter>,
        page_offset: i64,
        page_size: usize,
    ) -> LibraryItemPage {
        if page_offset < 0
            || page_size == 0
            || page_offset > i32::MAX.into()
            || !self.is_root_available()
        {
            return LibraryItemPage::empty();
        }

        let root_path = self.root_path();
        let candidates = match self.enumerate_media_candidates(&root_path, filter) {
            Ok(candidates) => candidates,
            Err(_) => return LibraryItemPage::empty(),
        };

        let mut skipped_items = 0_i64;
        let mut items = Vec::with_capacity(page_size);
        let mut next_page_offset = None;
        for candidate in candidates {
            let Some(item) = self.try_create_library_item(&root_path, &candidate.path) else {
                continue;
            };

            if skipped_items < page_offset {
                skipped_items += 1;
                continue;
            }

            if items.len() < page_size {
                items.push(item);
                continue;
            }

            next_page_offset = page_offset.checked_add(page_size.try_into().unwrap_or(i64::MAX));
            break;
        }

        LibraryItemPage {
            items,
            next_page_offset,
        }
    }

    pub async fn get_item(&self, id: &str) -> Option<LibraryItem> {
        let media_file = self.resolve_media_file(id, VARIANT_ID)?;
        Some(self.create_library_item(&media_file))
    }

    pub async fn get_media_file(&self, item_id: &str, variant_id: &str) -> Option<MediaFile> {
        self.resolve_media_file(item_id, variant_id)
    }

    pub async fn open_media_file(
        &self,
        item_id: &str,
        variant_id: &str,
    ) -> Option<OpenedMediaFile> {
        let media_file = self.resolve_media_file(item_id, variant_id)?;
        let file = open_read_no_follow(&self.root_path(), &media_file.relative_path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }

        Some(OpenedMediaFile {
            file,
            content_type: media_file.content_type,
            last_modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            size_bytes: metadata.len(),
        })
    }

    pub async fn cache_root(&self) -> CacheRoot {
        let root_path = self.root_path();
        let writable = self.is_root_available() && can_write_to_directory(&root_path);
        let mut root = CacheRoot {
            id: ROOT_ID.to_owned(),
            label: "Local Cache".to_owned(),
            kind: CacheRootKind::LocalDirectory.into(),
            path: root_path.to_string_lossy().into_owned(),
            writable,
            free_bytes: 0,
            total_bytes: 0,
        };

        if writable && let Some((free_bytes, total_bytes)) = filesystem_capacity(&root_path) {
            root.free_bytes = free_bytes;
            root.total_bytes = total_bytes;
        }

        root
    }

    pub fn is_root_available(&self) -> bool {
        let root_path = self.root_path();
        fs::symlink_metadata(&root_path)
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false)
    }

    pub async fn count_items(&self) -> i32 {
        if !self.is_root_available() {
            return 0;
        }

        let root_path = self.root_path();
        let Ok(candidates) = self.enumerate_media_candidates(&root_path, None) else {
            return 0;
        };

        candidates
            .into_iter()
            .filter(|candidate| {
                self.try_create_media_file(&root_path, &candidate.path)
                    .is_some()
            })
            .take(i32::MAX as usize)
            .count()
            .try_into()
            .unwrap_or(i32::MAX)
    }

    fn create_library_item(&self, media_file: &MediaFile) -> LibraryItem {
        let mut item = LibraryItem {
            id: create_item_id(&media_file.relative_path),
            title: media_file
                .path
                .file_stem()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default(),
            subtitle: media_file.relative_path.clone(),
            source: LibrarySource::LocalCache.into(),
            source_id: media_file.relative_path.clone(),
            poster_uri: String::new(),
            variants: Vec::new(),
            created_at: Some(timestamp_from_system_time(media_file.created_at)),
            updated_at: Some(timestamp_from_system_time(media_file.last_modified)),
        };

        if self.supports_http_range_playback() {
            item.variants.push(MediaVariant {
                id: VARIANT_ID.to_owned(),
                label: "Original".to_owned(),
                protocol: PlaybackProtocol::HttpFile.into(),
                container: media_file
                    .path
                    .extension()
                    .map(|extension| {
                        extension
                            .to_string_lossy()
                            .trim_start_matches('.')
                            .to_lowercase()
                    })
                    .unwrap_or_default(),
                video_codec: String::new(),
                audio_codec: String::new(),
                width: 0,
                height: 0,
                bitrate: 0,
                size_bytes: media_file.size_bytes.try_into().unwrap_or(i64::MAX),
            });
        }

        item
    }

    fn resolve_media_file(&self, item_id: &str, variant_id: &str) -> Option<MediaFile> {
        if variant_id != VARIANT_ID {
            return None;
        }

        let relative_path = decode_item_id(item_id)?;
        self.try_create_media_file(&self.root_path(), &self.root_path().join(relative_path))
    }

    fn try_create_library_item(&self, root_path: &Path, path: &Path) -> Option<LibraryItem> {
        let media_file = self.try_create_media_file(root_path, path)?;
        Some(self.create_library_item(&media_file))
    }

    fn try_create_media_file(&self, root_path: &Path, candidate_path: &Path) -> Option<MediaFile> {
        let full_candidate_path = absolute_path(candidate_path);
        if !is_within_root(root_path, &full_candidate_path) {
            return None;
        }

        let metadata = fs::symlink_metadata(&full_candidate_path).ok()?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return None;
        }

        if path_contains_link(root_path, &full_candidate_path) {
            return None;
        }

        if !self
            .allowed_extensions()
            .contains(&extension_with_dot(&full_candidate_path))
        {
            return None;
        }

        let relative_path = relative_path(root_path, &full_candidate_path)?;
        if !self.supports_http_range_playback() {
            return Some(MediaFile {
                path: full_candidate_path,
                relative_path,
                content_type: content_type(candidate_path).to_owned(),
                created_at: UNIX_EPOCH,
                last_modified: UNIX_EPOCH,
                size_bytes: 0,
            });
        }

        let file = open_read_no_follow(root_path, &relative_path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }

        Some(MediaFile {
            path: full_candidate_path,
            relative_path,
            content_type: content_type(candidate_path).to_owned(),
            created_at: metadata.created().unwrap_or(UNIX_EPOCH),
            last_modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            size_bytes: metadata.len(),
        })
    }

    fn enumerate_media_candidates(
        &self,
        root_path: &Path,
        filter: Option<&LibraryFilter>,
    ) -> io::Result<Vec<MediaCandidate>> {
        if let Some(filter) = filter {
            let requested_sources = filter.sources.to_vec();
            if !requested_sources.is_empty()
                && !requested_sources.contains(&(LibrarySource::LocalCache as i32))
            {
                return Ok(Vec::new());
            }
        }

        let allowed_extensions = self.allowed_extensions();
        let search_text = filter
            .map(|filter| filter.search_text.trim().to_lowercase())
            .filter(|text| !text.is_empty());
        let mut candidates = Vec::new();
        collect_media_candidates(
            root_path,
            root_path,
            &allowed_extensions,
            &search_text,
            &mut candidates,
        )?;
        candidates.sort();
        candidates.dedup_by(|left, right| left.path == right.path);
        Ok(candidates)
    }

    fn allowed_extensions(&self) -> Vec<String> {
        self.options
            .allowed_extensions
            .iter()
            .map(|extension| {
                let extension = extension.trim().to_lowercase();
                if extension.starts_with('.') {
                    extension
                } else {
                    format!(".{extension}")
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct MediaFile {
    pub path: PathBuf,
    pub relative_path: String,
    pub content_type: String,
    pub created_at: SystemTime,
    pub last_modified: SystemTime,
    pub size_bytes: u64,
}

pub struct OpenedMediaFile {
    pub file: File,
    pub content_type: String,
    pub last_modified: SystemTime,
    pub size_bytes: u64,
}

pub struct LibraryItemPage {
    pub items: Vec<LibraryItem>,
    pub next_page_offset: Option<i64>,
}

impl LibraryItemPage {
    fn empty() -> Self {
        Self {
            items: Vec::new(),
            next_page_offset: None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct MediaCandidate {
    path: PathBuf,
    title: String,
    subtitle: String,
}

impl Ord for MediaCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.title
            .to_lowercase()
            .cmp(&other.title.to_lowercase())
            .then_with(|| {
                self.subtitle
                    .to_lowercase()
                    .cmp(&other.subtitle.to_lowercase())
            })
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl PartialOrd for MediaCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn collect_media_candidates(
    root_path: &Path,
    directory: &Path,
    allowed_extensions: &[String],
    search_text: &Option<String>,
    candidates: &mut Vec<MediaCandidate>,
) -> io::Result<()> {
    for entry in match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    } {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            collect_media_candidates(
                root_path,
                &path,
                allowed_extensions,
                search_text,
                candidates,
            )?;
            continue;
        }

        if !file_type.is_file() || !allowed_extensions.contains(&extension_with_dot(&path)) {
            continue;
        }

        let Some(subtitle) = relative_path(root_path, &path) else {
            continue;
        };
        let title = path
            .file_stem()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(search_text) = search_text
            && !title.to_lowercase().contains(search_text)
            && !subtitle.to_lowercase().contains(search_text)
        {
            continue;
        }

        candidates.push(MediaCandidate {
            path,
            title,
            subtitle,
        });
    }

    Ok(())
}

fn create_item_id(relative_path: &str) -> String {
    format!(
        "local.{ROOT_ID}.{}",
        URL_SAFE_NO_PAD.encode(relative_path.as_bytes())
    )
}

pub fn decode_item_id(item_id: &str) -> Option<PathBuf> {
    let prefix = format!("local.{ROOT_ID}.");
    let encoded = item_id.strip_prefix(&prefix)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let relative_path = String::from_utf8(bytes).ok()?;
    let path = PathBuf::from(relative_path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::CurDir
                    | Component::RootDir
                    | Component::Prefix(_)
            )
        })
    {
        return None;
    }

    Some(path)
}

fn relative_path(root_path: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root_path).ok()?;
    Some(
        relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"),
    )
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

fn extension_with_dot(path: &Path) -> String {
    path.extension()
        .map(|extension| format!(".{}", extension.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

fn content_type(path: &Path) -> &'static str {
    match extension_with_dot(path).as_str() {
        ".m4v" => "video/x-m4v",
        ".mov" => "video/quicktime",
        _ => "video/mp4",
    }
}

fn timestamp_from_system_time(value: SystemTime) -> Timestamp {
    let duration = value.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos().try_into().unwrap_or(i32::MAX),
    }
}

fn path_contains_link(root_path: &Path, candidate_path: &Path) -> bool {
    let full_root_path = absolute_path(root_path);
    let full_candidate_path = absolute_path(candidate_path);
    if is_link(&full_root_path) || !is_within_root(&full_root_path, &full_candidate_path) {
        return true;
    }

    let mut current_path = full_candidate_path;
    while current_path != full_root_path {
        if is_link(&current_path) {
            return true;
        }

        let Some(parent) = current_path.parent() else {
            return true;
        };
        current_path = parent.to_path_buf();
    }

    false
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(true)
}

fn is_within_root(root_path: &Path, candidate_path: &Path) -> bool {
    let root_path = absolute_path(root_path);
    let candidate_path = absolute_path(candidate_path);
    candidate_path == root_path || candidate_path.starts_with(root_path)
}

#[cfg(target_os = "macos")]
fn supports_secure_no_follow_open() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
fn supports_secure_no_follow_open() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn open_read_no_follow(root_path: &Path, relative_path: &str) -> io::Result<File> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

    let segments = Path::new(relative_path)
        .components()
        .map(|component| match component {
            Component::Normal(value) => CString::new(value.as_bytes()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "path segment contains NUL")
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "relative path contains non-normal segment",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    if segments.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "media file relative path is empty",
        ));
    }

    let mut directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY,
    )?;
    for segment in &segments[..segments.len() - 1] {
        directory = open_at(
            directory.as_raw_fd(),
            segment,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        )?;
    }

    open_at(
        directory.as_raw_fd(),
        segments.last().expect("segments is not empty"),
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )
}

#[cfg(not(target_os = "macos"))]
fn open_read_no_follow(_root_path: &Path, _relative_path: &str) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure no-follow media open is not implemented on this platform",
    ))
}

#[cfg(target_os = "macos")]
fn open_path(path: &Path, flags: i32) -> io::Result<File> {
    use std::os::{fd::FromRawFd, unix::ffi::OsStrExt};

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: libc open returns a fresh file descriptor or -1. Ownership is transferred to File only on success.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: fd was returned by open and is owned by this function on success.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "macos")]
fn open_at(directory_fd: i32, path: &CString, flags: i32) -> io::Result<File> {
    use std::os::fd::FromRawFd;

    // SAFETY: directory_fd is borrowed from a live File and path is a valid C string.
    let fd = unsafe { libc::openat(directory_fd, path.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: fd was returned by openat and is owned by this function on success.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn can_write_to_directory(path: &Path) -> bool {
    let probe_path = path.join(format!(
        ".tvnp-write-probe-{}",
        uuid::Uuid::new_v4().simple()
    ));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)
    {
        Ok(_) => {
            let _ = fs::remove_file(probe_path);
            true
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn filesystem_capacity(path: &Path) -> Option<(i64, i64)> {
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: stats points to valid writable memory and path is a valid C string.
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return None;
    }

    // SAFETY: statvfs returned success and initialized stats.
    let stats = unsafe { stats.assume_init() };
    let fragment_size = stats.f_frsize as i64;
    let free = (stats.f_bavail as i64).saturating_mul(fragment_size);
    let total = (stats.f_blocks as i64).saturating_mul(fragment_size);
    Some((free, total))
}

#[cfg(not(unix))]
fn filesystem_capacity(_path: &Path) -> Option<(i64, i64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_ids_round_trip() {
        let relative_path = "Movies/Sample Clip.mp4";
        let item_id = create_item_id(relative_path);
        assert_eq!(
            PathBuf::from(relative_path),
            decode_item_id(&item_id).unwrap()
        );
    }

    #[test]
    fn invalid_item_ids_are_rejected() {
        assert!(decode_item_id("bad").is_none());
        assert!(decode_item_id(&create_item_id("../escape.mp4")).is_none());
        assert!(decode_item_id(&create_item_id("/escape.mp4")).is_none());
        assert!(decode_item_id(&create_item_id("./escape.mp4")).is_none());
    }
}
