use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ffi::{CString, OsStr},
    fs::{self, File},
    io,
    path::{Component, Components, Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, RwLock as StdRwLock, RwLockReadGuard as StdRwLockReadGuard, Weak,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use prost_types::Timestamp;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, Semaphore};

use crate::{
    config::CacheServerOptions,
    generated::tvos_net_player::v1::{
        CacheRoot, CacheRootKind, LibraryFilter, LibraryItem, LibrarySource, MediaVariant,
        PlaybackProtocol,
    },
};

pub const ROOT_ID: &str = "default";
pub const VARIANT_ID: &str = "original";
const MAX_BLOCKING_LIBRARY_JOBS: usize = 4;
const MAX_PUBLICATION_GATE_STABILIZATION_ATTEMPTS: usize = 8;
const INTERNAL_CACHE_DIR: &str = ".tvos-net-player";

pub(crate) struct LibraryPublicationGate {
    state: StdRwLock<Arc<LibraryPublicationGateState>>,
    managed_output_prefix: Option<PathBuf>,
}

enum LibraryPublicationGateState {
    Unknown { blocked_prefix: PathBuf },
    Known { blocked_paths: HashSet<PathBuf> },
}

impl LibraryPublicationGate {
    fn known_empty() -> Self {
        Self {
            state: StdRwLock::new(Arc::new(LibraryPublicationGateState::Known {
                blocked_paths: HashSet::new(),
            })),
            managed_output_prefix: None,
        }
    }

    pub(crate) fn unknown_for_output_directory(
        cache_root: &Path,
        output_directory: &Path,
    ) -> io::Result<Self> {
        let relative_path = output_directory.strip_prefix(cache_root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Bilibili output directory is outside the cache root",
            )
        })?;
        let blocked_prefix = if relative_path.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            normalized_publication_gate_path(relative_path)?
        };
        Ok(Self {
            state: StdRwLock::new(Arc::new(LibraryPublicationGateState::Unknown {
                blocked_prefix: blocked_prefix.clone(),
            })),
            managed_output_prefix: Some(blocked_prefix),
        })
    }

    pub(crate) fn managed_output_prefix(&self) -> Option<PathBuf> {
        self.managed_output_prefix.clone()
    }

    pub(crate) fn install_durable_blocked_paths<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> io::Result<()> {
        let blocked_paths = paths
            .into_iter()
            .map(|path| normalized_publication_gate_path(Path::new(path)))
            .collect::<io::Result<HashSet<_>>>()?;
        let mut current = self
            .state
            .write()
            .expect("library publication gate lock poisoned");
        if matches!(
            current.as_ref(),
            LibraryPublicationGateState::Known {
                blocked_paths: current_paths,
            } if current_paths == &blocked_paths
        ) {
            return Ok(());
        }
        *current = Arc::new(LibraryPublicationGateState::Known { blocked_paths });
        Ok(())
    }

    fn snapshot(&self) -> Arc<LibraryPublicationGateState> {
        self.state
            .read()
            .expect("library publication gate lock poisoned")
            .clone()
    }

    fn read_if_current(
        &self,
        snapshot: &Arc<LibraryPublicationGateState>,
    ) -> Option<StdRwLockReadGuard<'_, Arc<LibraryPublicationGateState>>> {
        let current = self
            .state
            .read()
            .expect("library publication gate lock poisoned");
        Arc::ptr_eq(&current, snapshot).then_some(current)
    }

    fn with_stable_snapshot<T>(
        &self,
        mut operation: impl FnMut(&LibraryPublicationGateState) -> T,
    ) -> Option<T> {
        // Arc identity is the publication-state generation token. Filesystem metadata may
        // legitimately change during a scan; no-follow object and access-policy checks remain
        // responsible for those independent properties.
        for _ in 0..MAX_PUBLICATION_GATE_STABILIZATION_ATTEMPTS {
            let snapshot = self.snapshot();
            let result = operation(&snapshot);
            if self.read_if_current(&snapshot).is_some() {
                return Some(result);
            }
        }
        None
    }
}

impl LibraryPublicationGateState {
    fn blocks(&self, relative_path: &Path) -> bool {
        match self {
            Self::Unknown { blocked_prefix } => relative_path.starts_with(blocked_prefix),
            Self::Known { blocked_paths } => relative_path
                .ancestors()
                .filter(|path| !path.as_os_str().is_empty())
                .any(|ancestor| blocked_paths.contains(ancestor)),
        }
    }
}

#[derive(Clone)]
pub struct LocalMediaLibrary {
    options: Arc<CacheServerOptions>,
    blocking_jobs: Arc<Semaphore>,
    item_mutation_locks: Arc<StdMutex<HashMap<String, Weak<RwLock<()>>>>>,
    publication_gate: Arc<LibraryPublicationGate>,
}

impl LocalMediaLibrary {
    pub fn new(options: Arc<CacheServerOptions>) -> Self {
        Self::new_with_publication_gate(options, Arc::new(LibraryPublicationGate::known_empty()))
    }

    pub(crate) fn new_with_publication_gate(
        options: Arc<CacheServerOptions>,
        publication_gate: Arc<LibraryPublicationGate>,
    ) -> Self {
        Self {
            options,
            blocking_jobs: Arc::new(Semaphore::new(MAX_BLOCKING_LIBRARY_JOBS)),
            item_mutation_locks: Arc::new(StdMutex::new(HashMap::new())),
            publication_gate,
        }
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
        let filter = filter.cloned();
        self.run_blocking(move |library, cancellation| {
            library.list_items_page_blocking(filter.as_ref(), page_offset, page_size, cancellation)
        })
        .await
    }

    pub async fn get_item(&self, id: &str) -> Option<LibraryItem> {
        let id = id.to_owned();
        self.run_blocking(move |library, _| library.get_item_blocking(&id))
            .await
    }

    pub async fn item_id_for_media_path(&self, path: impl Into<PathBuf>) -> Option<String> {
        let path = path.into();
        self.run_blocking(move |library, _| library.item_id_for_media_path_blocking(&path))
            .await
    }

    pub async fn reserve_media_path_for_publication(
        &self,
        path: impl Into<PathBuf>,
    ) -> Option<LibraryItemPublicationLease> {
        let path = path.into();
        let item_id = self.item_id_for_media_path(path.clone()).await?;
        let guard = self.acquire_item_publication_guard(&item_id).await;
        (self.item_id_for_media_path(path).await.as_deref() == Some(item_id.as_str())).then_some(
            LibraryItemPublicationLease {
                item_id,
                _guard: guard,
            },
        )
    }

    pub async fn get_media_file(&self, item_id: &str, variant_id: &str) -> Option<MediaFile> {
        let item_id = item_id.to_owned();
        let variant_id = variant_id.to_owned();
        self.run_blocking(move |library, _| library.get_media_file_blocking(&item_id, &variant_id))
            .await
    }

    pub async fn open_media_file(
        &self,
        item_id: &str,
        variant_id: &str,
    ) -> Option<OpenedMediaFile> {
        let item_id = item_id.to_owned();
        let variant_id = variant_id.to_owned();
        self.run_blocking(move |library, _| library.open_media_file_blocking(&item_id, &variant_id))
            .await
    }

    pub async fn cache_root(&self) -> CacheRoot {
        self.run_blocking(|library, _| library.cache_root_blocking())
            .await
    }

    pub async fn delete_item(&self, id: &str) -> io::Result<bool> {
        let Some(deletion) = self.prepare_item_deletion(id).await? else {
            return Ok(false);
        };
        deletion.delete().await
    }

    pub async fn prepare_item_deletion(&self, id: &str) -> io::Result<Option<LibraryItemDeletion>> {
        let requested_id = id.to_owned();
        let Some((item_id, relative_path)) = self
            .run_blocking(move |library, _| {
                library.canonical_deletable_item_id_blocking(&requested_id)
            })
            .await?
        else {
            return Ok(None);
        };
        let guard = self.acquire_item_deletion_guard(&item_id).await;
        Ok(Some(LibraryItemDeletion {
            library: self.clone(),
            item_id,
            relative_path,
            _guard: guard,
        }))
    }

    pub async fn is_root_available(&self) -> bool {
        self.run_blocking(|library, _| library.is_root_available_blocking())
            .await
    }

    pub async fn count_items(&self) -> i32 {
        self.run_blocking(|library, cancellation| library.count_items_blocking(cancellation))
            .await
    }

    async fn run_blocking<T, F>(&self, job: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(Self, BlockingCancellation) -> T + Send + 'static,
    {
        let permit = Arc::clone(&self.blocking_jobs)
            .acquire_owned()
            .await
            .expect("library blocking semaphore must stay open");
        let library = self.clone();
        let cancellation = BlockingCancellation::default();
        let cancellation_guard = cancellation.guard();
        let worker_cancellation = cancellation.clone();

        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            job(library, worker_cancellation)
        })
        .await
        .expect("library blocking task panicked");
        drop(cancellation_guard);
        result
    }

    async fn acquire_item_publication_guard(&self, item_id: &str) -> OwnedRwLockReadGuard<()> {
        self.item_mutation_lock(item_id).read_owned().await
    }

    async fn acquire_item_deletion_guard(&self, item_id: &str) -> OwnedRwLockWriteGuard<()> {
        self.item_mutation_lock(item_id).write_owned().await
    }

    fn item_mutation_lock(&self, item_id: &str) -> Arc<RwLock<()>> {
        {
            let mut locks = self
                .item_mutation_locks
                .lock()
                .expect("library item mutation lock map poisoned");
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(item_id).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(RwLock::new(()));
                locks.insert(item_id.to_owned(), Arc::downgrade(&lock));
                lock
            }
        }
    }

    fn list_items_page_blocking(
        &self,
        filter: Option<&LibraryFilter>,
        page_offset: i64,
        page_size: usize,
        cancellation: BlockingCancellation,
    ) -> LibraryItemPage {
        self.list_items_page_blocking_with_scan_observer(
            filter,
            page_offset,
            page_size,
            cancellation,
            || {},
        )
    }

    fn list_items_page_blocking_with_scan_observer(
        &self,
        filter: Option<&LibraryFilter>,
        page_offset: i64,
        page_size: usize,
        cancellation: BlockingCancellation,
        mut after_scan: impl FnMut(),
    ) -> LibraryItemPage {
        if page_offset < 0
            || page_size == 0
            || page_offset > i32::MAX.into()
            || !self.is_root_available_blocking()
        {
            return LibraryItemPage::empty();
        }

        let root_path = self.root_path();
        self.publication_gate
            .with_stable_snapshot(|publication_gate| {
                // Candidate pruning and page projection must use the same publication
                // generation so an ownership release cannot make an incomplete scan current.
                let candidates = match self.enumerate_media_candidates(
                    &root_path,
                    filter,
                    &cancellation,
                    publication_gate,
                ) {
                    Ok(candidates) => candidates,
                    Err(_) => return LibraryItemPage::empty(),
                };
                after_scan();

                let mut skipped_items = 0_i64;
                let mut items = Vec::with_capacity(page_size);
                let mut next_page_offset = None;
                for candidate in &candidates {
                    if cancellation.is_cancelled() {
                        return LibraryItemPage::empty();
                    }
                    let Some(item) = self.try_create_library_item(&root_path, &candidate.path)
                    else {
                        continue;
                    };
                    if publication_gate.blocks(Path::new(&item.source_id)) {
                        continue;
                    }

                    if skipped_items < page_offset {
                        skipped_items += 1;
                        continue;
                    }

                    if items.len() < page_size {
                        items.push(item);
                        continue;
                    }

                    next_page_offset =
                        page_offset.checked_add(page_size.try_into().unwrap_or(i64::MAX));
                    break;
                }

                LibraryItemPage {
                    items,
                    next_page_offset,
                }
            })
            .unwrap_or_else(LibraryItemPage::empty)
    }

    fn get_item_blocking(&self, id: &str) -> Option<LibraryItem> {
        self.publication_gate
            .with_stable_snapshot(|publication_gate| {
                let media_file = self.resolve_media_file(id, VARIANT_ID, publication_gate)?;
                Some(self.create_library_item(&media_file))
            })
            .flatten()
    }

    fn item_id_for_media_path_blocking(&self, path: &Path) -> Option<String> {
        let root_path = self.root_path();
        let media_file = self.try_create_media_file(&root_path, path)?;
        Some(create_item_id(&media_file.relative_path))
    }

    fn get_media_file_blocking(&self, item_id: &str, variant_id: &str) -> Option<MediaFile> {
        self.publication_gate
            .with_stable_snapshot(|publication_gate| {
                self.resolve_media_file(item_id, variant_id, publication_gate)
            })
            .flatten()
    }

    fn open_media_file_blocking(&self, item_id: &str, variant_id: &str) -> Option<OpenedMediaFile> {
        self.publication_gate
            .with_stable_snapshot(|publication_gate| {
                let media_file = self.resolve_media_file(item_id, variant_id, publication_gate)?;
                let file =
                    open_read_no_follow(&self.root_path(), &media_file.relative_path).ok()?;
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
            })
            .flatten()
    }

    fn cache_root_blocking(&self) -> CacheRoot {
        let root_path = self.root_path();
        let writable = self.is_root_available_blocking() && can_write_to_directory(&root_path);
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

    fn delete_item_blocking(&self, id: &str) -> io::Result<bool> {
        for _ in 0..MAX_PUBLICATION_GATE_STABILIZATION_ATTEMPTS {
            let publication_gate = self.publication_gate.snapshot();
            let media_file = self.resolve_deletable_media_file(id, &publication_gate)?;
            let Some(_current_gate) = self.publication_gate.read_if_current(&publication_gate)
            else {
                continue;
            };
            let Some(media_file) = media_file else {
                return Ok(false);
            };
            // Keep the short generation guard across the irreversible unlink. Directory scans
            // and canonicalization have already completed without holding the publication lock.
            return remove_file_no_follow(&self.root_path(), &media_file.relative_path);
        }
        Err(publication_gate_changed_error())
    }

    fn is_root_available_blocking(&self) -> bool {
        let root_path = self.root_path();
        fs::metadata(&root_path)
            .map(|metadata| metadata.is_dir() && !path_has_link_component(&root_path))
            .unwrap_or(false)
    }

    fn count_items_blocking(&self, cancellation: BlockingCancellation) -> i32 {
        self.count_items_blocking_with_scan_observer(cancellation, || {})
    }

    fn count_items_blocking_with_scan_observer(
        &self,
        cancellation: BlockingCancellation,
        mut after_scan: impl FnMut(),
    ) -> i32 {
        if !self.is_root_available_blocking() {
            return 0;
        }

        let root_path = self.root_path();
        self.publication_gate
            .with_stable_snapshot(|publication_gate| {
                let Ok(candidates) = self.enumerate_media_candidates(
                    &root_path,
                    None,
                    &cancellation,
                    publication_gate,
                ) else {
                    return 0;
                };
                after_scan();

                candidates
                    .iter()
                    .filter_map(|candidate| self.try_create_media_file(&root_path, &candidate.path))
                    .filter(|media_file| {
                        !publication_gate.blocks(Path::new(&media_file.relative_path))
                    })
                    .take(i32::MAX as usize)
                    .count()
                    .try_into()
                    .unwrap_or(i32::MAX)
            })
            .unwrap_or(0)
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

    fn resolve_media_file(
        &self,
        item_id: &str,
        variant_id: &str,
        publication_gate: &LibraryPublicationGateState,
    ) -> Option<MediaFile> {
        if variant_id != VARIANT_ID {
            return None;
        }

        let relative_path = decode_item_id(item_id)?;
        let media_file =
            self.try_create_media_file(&self.root_path(), &self.root_path().join(relative_path))?;
        if publication_gate.blocks(Path::new(&media_file.relative_path)) {
            return None;
        }
        Some(media_file)
    }

    fn resolve_deletable_media_file(
        &self,
        item_id: &str,
        publication_gate: &LibraryPublicationGateState,
    ) -> io::Result<Option<MediaFile>> {
        let root_path = self.root_path();
        ensure_deletable_root(&root_path)?;

        let Some(decoded_relative_path) = decode_item_id(item_id) else {
            return Ok(None);
        };
        let full_candidate_path = absolute_path(&root_path.join(decoded_relative_path));
        if !is_within_root(&root_path, &full_candidate_path) {
            return Ok(None);
        }

        let metadata = match fs::symlink_metadata(&full_candidate_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Ok(None);
        }

        if path_contains_link(&root_path, &full_candidate_path) {
            return Ok(None);
        }

        let Some((canonical_root_path, full_candidate_path, relative_path)) =
            canonical_existing_media_path(&root_path, &full_candidate_path)
        else {
            return Ok(None);
        };
        if publication_gate.blocks(Path::new(&relative_path)) {
            return Ok(None);
        }
        if !self
            .allowed_extensions()
            .contains(&extension_with_dot(&full_candidate_path))
        {
            return Ok(None);
        }

        if is_internal_cache_path(&relative_path) {
            return Ok(None);
        }
        let media_content_type = content_type(&full_candidate_path).to_owned();
        if !self.supports_http_range_playback() {
            return Ok(Some(MediaFile {
                path: full_candidate_path,
                relative_path,
                content_type: media_content_type,
                created_at: UNIX_EPOCH,
                last_modified: UNIX_EPOCH,
                size_bytes: 0,
            }));
        }

        let file = match open_read_no_follow(&canonical_root_path, &relative_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Ok(None);
        }

        Ok(Some(MediaFile {
            path: full_candidate_path,
            relative_path,
            content_type: media_content_type,
            created_at: metadata.created().unwrap_or(UNIX_EPOCH),
            last_modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            size_bytes: metadata.len(),
        }))
    }

    fn canonical_deletable_item_id_blocking(
        &self,
        item_id: &str,
    ) -> io::Result<Option<(String, String)>> {
        self.publication_gate
            .with_stable_snapshot(|publication_gate| {
                self.canonical_deletable_item_id_for_snapshot(item_id, publication_gate)
            })
            .unwrap_or_else(|| Err(publication_gate_changed_error()))
    }

    fn canonical_deletable_item_id_for_snapshot(
        &self,
        item_id: &str,
        publication_gate: &LibraryPublicationGateState,
    ) -> io::Result<Option<(String, String)>> {
        let root_path = self.root_path();
        ensure_deletable_root(&root_path)?;
        let Some(relative_path) = decode_item_id(item_id) else {
            return Ok(None);
        };
        let relative_path = relative_path.components().collect::<PathBuf>();
        if relative_path.as_os_str().is_empty() {
            return Ok(None);
        }
        if publication_gate.blocks(&relative_path) {
            return Ok(None);
        }
        let full_candidate_path = absolute_path(&root_path.join(&relative_path));
        if !is_within_root(&root_path, &full_candidate_path)
            || is_internal_cache_components(relative_path.components())
        {
            return Ok(None);
        }
        match fs::symlink_metadata(&full_candidate_path) {
            Ok(_) => {
                return self
                    .resolve_deletable_media_file(item_id, publication_gate)
                    .map(|media_file| {
                        media_file.map(|media_file| {
                            let item_id = create_item_id(&media_file.relative_path);
                            (item_id, media_file.relative_path)
                        })
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if !self
            .allowed_extensions()
            .contains(&extension_with_dot(&full_candidate_path))
        {
            return Ok(None);
        }
        // Existing parents must preserve directory-only, no-follow access policy. Missing
        // descendants are allowed so durable task references can still be tombstoned after an
        // out-of-process removal; the later unlink path performs its own no-follow validation.
        if existing_relative_parent_has_unsafe_component(&root_path, &relative_path)? {
            return Ok(None);
        }
        let relative_path = relative_path.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "library item id is not valid UTF-8",
            )
        })?;
        Ok(Some((
            create_item_id(relative_path),
            relative_path.to_owned(),
        )))
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

        // Canonical spelling supplies one logical identity for filesystem aliases. The later
        // descriptor-relative no-follow open independently enforces the access policy.
        let (canonical_root_path, full_candidate_path, relative_path) =
            canonical_existing_media_path(root_path, &full_candidate_path)?;

        if !self
            .allowed_extensions()
            .contains(&extension_with_dot(&full_candidate_path))
        {
            return None;
        }

        if is_internal_cache_path(&relative_path) {
            return None;
        }
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

        let file = open_read_no_follow(&canonical_root_path, &relative_path).ok()?;
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
        cancellation: &BlockingCancellation,
        publication_gate: &LibraryPublicationGateState,
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
            cancellation,
            publication_gate,
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

fn publication_gate_changed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "library publication state changed repeatedly",
    )
}

/// Keeps API-owned deletion from invalidating a canonical library item during publication.
/// The lease protects logical availability, not content or filesystem object identity against
/// out-of-process replacement; serving still uses the existing no-follow validation.
pub struct LibraryItemPublicationLease {
    pub item_id: String,
    _guard: OwnedRwLockReadGuard<()>,
}

pub struct LibraryItemDeletion {
    library: LocalMediaLibrary,
    item_id: String,
    relative_path: String,
    _guard: OwnedRwLockWriteGuard<()>,
}

impl LibraryItemDeletion {
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    pub(crate) fn relative_path(&self) -> &str {
        &self.relative_path
    }

    pub async fn delete(self) -> io::Result<bool> {
        let item_id = self.item_id.clone();
        self.library
            .run_blocking(move |library, _| library.delete_item_blocking(&item_id))
            .await
    }
}

#[derive(Clone, Default)]
struct BlockingCancellation {
    cancelled: Arc<AtomicBool>,
}

impl BlockingCancellation {
    fn guard(&self) -> BlockingCancellationGuard {
        BlockingCancellationGuard {
            cancelled: Arc::clone(&self.cancelled),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(AtomicOrdering::Relaxed)
    }
}

struct BlockingCancellationGuard {
    cancelled: Arc<AtomicBool>,
}

impl Drop for BlockingCancellationGuard {
    fn drop(&mut self) {
        self.cancelled.store(true, AtomicOrdering::Relaxed);
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
    cancellation: &BlockingCancellation,
    publication_gate: &LibraryPublicationGateState,
    candidates: &mut Vec<MediaCandidate>,
) -> io::Result<()> {
    if cancellation.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "library scan cancelled",
        ));
    }

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
        if cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "library scan cancelled",
            ));
        }

        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(entry_relative_path) = path.strip_prefix(root_path) else {
            continue;
        };
        if publication_gate.blocks(entry_relative_path) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            if is_internal_cache_dir(root_path, &path) {
                continue;
            }
            collect_media_candidates(
                root_path,
                &path,
                allowed_extensions,
                search_text,
                cancellation,
                publication_gate,
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

fn is_internal_cache_dir(root_path: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root_path) else {
        return false;
    };
    is_internal_cache_components(relative.components())
}

fn is_internal_cache_path(relative_path: &str) -> bool {
    is_internal_cache_components(Path::new(relative_path).components())
}

fn is_internal_cache_components(mut components: Components<'_>) -> bool {
    matches!(
        components.next(),
        Some(Component::Normal(value))
            if value.eq_ignore_ascii_case(OsStr::new(INTERNAL_CACHE_DIR))
    )
}

pub(crate) fn create_item_id(relative_path: &str) -> String {
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

fn normalized_publication_gate_path(path: &Path) -> io::Result<PathBuf> {
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_os_string()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "library publication gate path is not a normal relative path",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    if components.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "library publication gate path cannot be empty",
        ));
    }
    Ok(components.into_iter().collect())
}

fn relative_path(root_path: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root_path).ok()?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::CurDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }

    Some(
        relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"),
    )
}

fn canonical_existing_media_path(
    root_path: &Path,
    candidate_path: &Path,
) -> Option<(PathBuf, PathBuf, String)> {
    let canonical_root_path = fs::canonicalize(root_path).ok()?;
    let canonical_candidate_path = fs::canonicalize(candidate_path).ok()?;
    if !is_within_root(&canonical_root_path, &canonical_candidate_path) {
        return None;
    }
    let relative_path = relative_path(&canonical_root_path, &canonical_candidate_path)?;
    Some((canonical_root_path, canonical_candidate_path, relative_path))
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
        ".m4a" => "audio/mp4",
        ".m4v" => "video/x-m4v",
        ".mov" => "video/quicktime",
        _ => "video/mp4",
    }
}

fn ensure_deletable_root(root_path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(root_path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("cache root is not a directory: {}", root_path.display()),
        ));
    }
    if path_has_link_component(root_path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "cache root contains a symbolic link component: {}",
                root_path.display()
            ),
        ));
    }

    Ok(())
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
    if path_has_link_component(&full_root_path)
        || !is_within_root(&full_root_path, &full_candidate_path)
    {
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

fn existing_relative_parent_has_unsafe_component(
    root_path: &Path,
    relative_path: &Path,
) -> io::Result<bool> {
    let Some(parent) = relative_path.parent() else {
        return Ok(false);
    };
    let mut current = root_path.to_path_buf();
    for component in parent.components() {
        let Component::Normal(component) = component else {
            return Ok(true);
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn path_has_link_component(path: &Path) -> bool {
    let full_path = absolute_path(path);
    let mut current_path = PathBuf::new();

    for component in full_path.components() {
        current_path.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        if is_link(&current_path) {
            return true;
        }
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

#[cfg(unix)]
fn supports_secure_no_follow_open() -> bool {
    true
}

#[cfg(not(unix))]
fn supports_secure_no_follow_open() -> bool {
    false
}

#[cfg(unix)]
pub(crate) fn open_read_no_follow(root_path: &Path, relative_path: &str) -> io::Result<File> {
    let root_directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    open_read_no_follow_at(&root_directory, relative_path)
}

#[cfg(unix)]
pub(crate) fn open_read_no_follow_at(
    root_directory: &File,
    relative_path: &str,
) -> io::Result<File> {
    use std::os::fd::AsRawFd;

    let segments = relative_path_segments(relative_path)?;
    let mut directory = root_directory.try_clone()?;
    for segment in &segments[..segments.len() - 1] {
        directory = open_at(
            directory.as_raw_fd(),
            segment,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )?;
    }

    open_at(
        directory.as_raw_fd(),
        segments.last().expect("segments is not empty"),
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
    )
}

#[cfg(not(unix))]
pub(crate) fn open_read_no_follow(_root_path: &Path, _relative_path: &str) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure no-follow media open is not implemented on this platform",
    ))
}

#[cfg(not(unix))]
pub(crate) fn open_read_no_follow_at(
    _root_directory: &File,
    _relative_path: &str,
) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure descriptor-relative media open is not implemented on this platform",
    ))
}

#[cfg(all(unix, test))]
pub(crate) fn list_directory_names_no_follow_bounded(
    root_path: &Path,
    relative_path: &str,
    max_names: usize,
) -> io::Result<Vec<String>> {
    list_optional_directory_names_no_follow_bounded(root_path, relative_path, max_names)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "managed directory does not exist"))
}

#[cfg(unix)]
pub(crate) fn list_optional_directory_names_no_follow_bounded(
    root_path: &Path,
    relative_path: &str,
    max_names: usize,
) -> io::Result<Option<Vec<String>>> {
    use std::os::fd::AsRawFd;

    let segments = relative_path_segments(relative_path)?;
    let mut directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY,
    )?;
    for segment in &segments {
        directory = match open_at(
            directory.as_raw_fd(),
            segment,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        ) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
    }

    list_open_directory_names_bounded(directory, max_names).map(Some)
}

#[cfg(unix)]
fn list_open_directory_names_bounded(directory: File, max_names: usize) -> io::Result<Vec<String>> {
    use std::{ffi::CStr, os::fd::IntoRawFd};

    let fd = directory.into_raw_fd();
    // SAFETY: fd is a uniquely owned open directory descriptor transferred to fdopendir.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so ownership of fd was not transferred.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let stream = DirectoryStream { stream };

    let mut names = Vec::new();
    let mut entry_count = 0_usize;
    loop {
        set_errno(0);
        // SAFETY: stream remains open and exclusively owned until closedir below.
        let entry = unsafe { libc::readdir(stream.stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(names)
            } else {
                Err(error)
            };
        }
        // SAFETY: readdir returns a live dirent whose d_name is NUL-terminated for this call.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if matches!(bytes, b"." | b"..") {
            continue;
        }
        entry_count = entry_count.saturating_add(1);
        if entry_count > max_names {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("directory entry limit exceeded: {max_names}"),
            ));
        }
        let name = std::str::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "managed directory contains a non-UTF-8 entry name",
            )
        })?;
        names.push(name.to_owned());
    }
}

#[cfg(unix)]
struct DirectoryStream {
    stream: *mut libc::DIR,
}

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: stream was returned by fdopendir and is owned by this wrapper.
        unsafe { libc::closedir(self.stream) };
    }
}

#[cfg(all(not(unix), test))]
pub(crate) fn list_directory_names_no_follow_bounded(
    _root_path: &Path,
    _relative_path: &str,
    _max_names: usize,
) -> io::Result<Vec<String>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure no-follow directory listing is not implemented on this platform",
    ))
}

#[cfg(not(unix))]
pub(crate) fn list_optional_directory_names_no_follow_bounded(
    _root_path: &Path,
    _relative_path: &str,
    _max_names: usize,
) -> io::Result<Option<Vec<String>>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure no-follow directory listing is not implemented on this platform",
    ))
}

#[cfg(all(unix, target_vendor = "apple"))]
fn set_errno(value: i32) {
    // SAFETY: __error returns the calling thread's errno pointer on Apple platforms.
    unsafe { *libc::__error() = value };
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn set_errno(value: i32) {
    // SAFETY: __errno_location returns the calling thread's errno pointer on supported Unix CI.
    unsafe { *libc::__errno_location() = value };
}

#[cfg(unix)]
pub(crate) fn remove_file_no_follow(root_path: &Path, relative_path: &str) -> io::Result<bool> {
    let root_directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    remove_file_no_follow_at(&root_directory, relative_path)
}

#[cfg(unix)]
pub(crate) fn remove_file_no_follow_at(
    root_directory: &File,
    relative_path: &str,
) -> io::Result<bool> {
    remove_entry_no_follow_at_with_parent_sync(root_directory, relative_path, 0, File::sync_all)
}

#[cfg(unix)]
fn remove_entry_no_follow_at_with_parent_sync(
    root_directory: &File,
    relative_path: &str,
    unlink_flags: i32,
    mut sync_parent: impl FnMut(&File) -> io::Result<()>,
) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    let segments = relative_path_segments(relative_path)?;
    let mut directory = root_directory.try_clone()?;
    for segment in &segments[..segments.len() - 1] {
        directory = match open_at(
            directory.as_raw_fd(),
            segment,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        ) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                sync_parent(&directory)?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
    }

    // The protected property is durable absence of this named entry beneath the verified
    // no-follow parent chain. The entry's previous object identity and content are irrelevant:
    // after unlink, or after observing an already-missing entry from a prior failed sync, the
    // containing directory must reach stable storage before cleanup ownership can be cleared.
    // SAFETY: directory fd is borrowed from a live File and the last path segment is a valid C string.
    let result = unsafe {
        libc::unlinkat(
            directory.as_raw_fd(),
            segments.last().expect("segments is not empty").as_ptr(),
            unlink_flags,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            sync_parent(&directory)?;
            Ok(false)
        } else {
            Err(error)
        };
    }

    sync_parent(&directory)?;
    Ok(true)
}

#[cfg(all(unix, test))]
fn remove_entry_no_follow_with_parent_sync(
    root_path: &Path,
    relative_path: &str,
    unlink_flags: i32,
    sync_parent: impl FnMut(&File) -> io::Result<()>,
) -> io::Result<bool> {
    let root_directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    remove_entry_no_follow_at_with_parent_sync(
        &root_directory,
        relative_path,
        unlink_flags,
        sync_parent,
    )
}

#[cfg(not(unix))]
pub(crate) fn remove_file_no_follow(root_path: &Path, relative_path: &str) -> io::Result<bool> {
    match fs::remove_file(root_path.join(relative_path)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
pub(crate) fn remove_file_no_follow_at(
    _root_directory: &File,
    _relative_path: &str,
) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure descriptor-relative file removal is not implemented on this platform",
    ))
}

#[cfg(unix)]
pub(crate) fn remove_empty_directory_no_follow(
    root_path: &Path,
    relative_path: &str,
) -> io::Result<bool> {
    let root_directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    remove_empty_directory_no_follow_at(&root_directory, relative_path)
}

#[cfg(unix)]
pub(crate) fn remove_empty_directory_no_follow_at(
    root_directory: &File,
    relative_path: &str,
) -> io::Result<bool> {
    // Protect path containment and no-follow access policy, not continuity of the leaf's
    // identity across calls: every parent is verified, and unlinkat removes only its named empty
    // child. A replacement that is not an empty directory fails instead of being traversed.
    remove_entry_no_follow_at_with_parent_sync(
        root_directory,
        relative_path,
        libc::AT_REMOVEDIR,
        File::sync_all,
    )
}

#[cfg(not(unix))]
pub(crate) fn remove_empty_directory_no_follow(
    _root_path: &Path,
    _relative_path: &str,
) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure no-follow directory removal is not implemented on this platform",
    ))
}

#[cfg(not(unix))]
pub(crate) fn remove_empty_directory_no_follow_at(
    _root_directory: &File,
    _relative_path: &str,
) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure descriptor-relative directory removal is not implemented on this platform",
    ))
}

#[cfg(unix)]
pub(crate) fn create_directory_exclusive_no_follow(
    root_path: &Path,
    relative_path: &str,
) -> io::Result<()> {
    create_directory_path_no_follow(root_path, relative_path, true)
}

#[cfg(not(unix))]
pub(crate) fn create_directory_exclusive_no_follow(
    _root_path: &Path,
    _relative_path: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure no-follow directory creation is not implemented on this platform",
    ))
}

#[cfg(unix)]
pub(crate) fn ensure_directory_no_follow(root_path: &Path, relative_path: &str) -> io::Result<()> {
    create_directory_path_no_follow(root_path, relative_path, false)
}

#[cfg(not(unix))]
pub(crate) fn ensure_directory_no_follow(
    _root_path: &Path,
    _relative_path: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure no-follow directory creation is not implemented on this platform",
    ))
}

#[cfg(unix)]
fn create_directory_path_no_follow(
    root_path: &Path,
    relative_path: &str,
    leaf_must_be_new: bool,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let segments = relative_path_segments(relative_path)?;
    let mut directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    for (index, segment) in segments.iter().enumerate() {
        let is_leaf = index + 1 == segments.len();
        // SAFETY: directory is a live directory descriptor and segment is NUL-terminated.
        let created = unsafe { libc::mkdirat(directory.as_raw_fd(), segment.as_ptr(), 0o700) };
        if created != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists || (is_leaf && leaf_must_be_new) {
                return Err(error);
            }
        }
        let child = open_at(
            directory.as_raw_fd(),
            segment,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )?;
        if created == 0 {
            child.sync_all()?;
            directory.sync_all()?;
        }
        directory = child;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn rename_directory_no_replace_no_follow(
    root_path: &Path,
    source_relative_path: &str,
    destination_relative_path: &str,
    max_entries: usize,
    max_depth: usize,
) -> io::Result<()> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

    if max_entries == 0 || max_depth == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory publication bounds must be positive",
        ));
    }

    let (source_parent, source_leaf) =
        open_relative_parent_no_follow(root_path, source_relative_path)?;
    let (destination_parent, destination_leaf) =
        open_relative_parent_no_follow(root_path, destination_relative_path)?;
    let source_directory = open_at(
        source_parent.as_raw_fd(),
        &source_leaf,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    let source_metadata = source_directory.metadata()?;
    let mut remaining_entries = max_entries;
    sync_open_directory_tree_no_follow(&source_directory, &mut remaining_entries, max_depth)?;

    rename_at_no_replace(
        source_parent.as_raw_fd(),
        &source_leaf,
        destination_parent.as_raw_fd(),
        &destination_leaf,
    )?;

    let destination_directory = open_at(
        destination_parent.as_raw_fd(),
        &destination_leaf,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    let destination_metadata = destination_directory.metadata()?;
    if source_metadata.dev() != destination_metadata.dev()
        || source_metadata.ino() != destination_metadata.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "renamed directory identity changed during publication",
        ));
    }

    destination_directory.sync_all()?;
    source_parent.sync_all()?;
    destination_parent.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn rename_directory_no_replace_no_follow(
    _root_path: &Path,
    _source_relative_path: &str,
    _destination_relative_path: &str,
    _max_entries: usize,
    _max_depth: usize,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure no-replace directory rename is not implemented on this platform",
    ))
}

#[cfg(unix)]
fn sync_open_directory_tree_no_follow(
    directory: &File,
    remaining_entries: &mut usize,
    remaining_depth: usize,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    if remaining_depth == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed directory depth limit exceeded",
        ));
    }
    let dot = CString::new(".").expect("dot contains no NUL");
    let listing = open_at(
        directory.as_raw_fd(),
        &dot,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    let names = list_open_directory_names_bounded(listing, *remaining_entries)?;
    for name in names {
        *remaining_entries = remaining_entries.checked_sub(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "managed directory entry limit exceeded",
            )
        })?;
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "managed entry name contains NUL",
            )
        })?;
        match open_at(
            directory.as_raw_fd(),
            &name,
            libc::O_RDONLY
                | libc::O_NONBLOCK
                | libc::O_NOFOLLOW
                | libc::O_DIRECTORY
                | libc::O_CLOEXEC,
        ) {
            Ok(child) => {
                sync_open_directory_tree_no_follow(&child, remaining_entries, remaining_depth - 1)?
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                let file = open_at(
                    directory.as_raw_fd(),
                    &name,
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )?;
                if !file.metadata()?.file_type().is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "managed directory contains an unsupported entry type",
                    ));
                }
                file.sync_all()?;
            }
            Err(error) => return Err(error),
        }
    }

    // BBDown has relinquished this task-owned tree before publication. Syncing every regular
    // file and directory protects the durability of those closed output bytes; it does not claim
    // content stability against an out-of-process writer that violates exclusive ownership.
    directory.sync_all()
}

#[cfg(all(unix, target_vendor = "apple"))]
fn rename_at_no_replace(
    source_parent_fd: i32,
    source_leaf: &CString,
    destination_parent_fd: i32,
    destination_leaf: &CString,
) -> io::Result<()> {
    // SAFETY: both descriptors are live directories and both names are NUL-terminated.
    let result = unsafe {
        libc::renameatx_np(
            source_parent_fd,
            source_leaf.as_ptr(),
            destination_parent_fd,
            destination_leaf.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(unix, target_os = "linux"))]
fn rename_at_no_replace(
    source_parent_fd: i32,
    source_leaf: &CString,
    destination_parent_fd: i32,
    destination_leaf: &CString,
) -> io::Result<()> {
    // SAFETY: both descriptors are live directories and both names are NUL-terminated.
    let result = unsafe {
        libc::renameat2(
            source_parent_fd,
            source_leaf.as_ptr(),
            destination_parent_fd,
            destination_leaf.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(unix, not(any(target_vendor = "apple", target_os = "linux"))))]
fn rename_at_no_replace(
    _source_parent_fd: i32,
    _source_leaf: &CString,
    _destination_parent_fd: i32,
    _destination_leaf: &CString,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace directory rename is not implemented on this platform",
    ))
}

#[cfg(unix)]
pub(crate) fn remove_directory_tree_no_follow(
    root_path: &Path,
    relative_path: &str,
    max_entries: usize,
    max_depth: usize,
) -> io::Result<bool> {
    let root_directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    remove_directory_tree_no_follow_at(&root_directory, relative_path, max_entries, max_depth)
}

#[cfg(unix)]
pub(crate) fn remove_directory_tree_no_follow_at(
    root_directory: &File,
    relative_path: &str,
    max_entries: usize,
    max_depth: usize,
) -> io::Result<bool> {
    remove_directory_tree_no_follow_at_with_parent_sync(
        root_directory,
        relative_path,
        max_entries,
        max_depth,
        File::sync_all,
    )
}

#[cfg(all(unix, test))]
fn remove_directory_tree_no_follow_with_parent_sync(
    root_path: &Path,
    relative_path: &str,
    max_entries: usize,
    max_depth: usize,
    sync_parent: impl FnMut(&File) -> io::Result<()>,
) -> io::Result<bool> {
    let root_directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    remove_directory_tree_no_follow_at_with_parent_sync(
        &root_directory,
        relative_path,
        max_entries,
        max_depth,
        sync_parent,
    )
}

#[cfg(unix)]
fn remove_directory_tree_no_follow_at_with_parent_sync(
    root_directory: &File,
    relative_path: &str,
    max_entries: usize,
    max_depth: usize,
    mut sync_parent: impl FnMut(&File) -> io::Result<()>,
) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    if max_entries == 0 || max_depth == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory cleanup bounds must be positive",
        ));
    }
    let segments = relative_path_segments(relative_path)?;
    let mut parent = root_directory.try_clone()?;
    for segment in &segments[..segments.len() - 1] {
        parent = match open_at(
            parent.as_raw_fd(),
            segment,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
        ) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                sync_parent(&parent)?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
    }
    let leaf = segments.last().expect("segments is not empty");
    let directory = match open_at(
        parent.as_raw_fd(),
        leaf,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    ) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            sync_parent(&parent)?;
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let mut remaining_entries = max_entries;
    remove_open_directory_contents_no_follow(&directory, &mut remaining_entries, max_depth)?;

    // The intent owns this logical directory pathname and its descendants. Traversal never
    // follows a symlink; object identity is not retained across calls because a replacement at
    // the still-owned name must either be removed safely or make cleanup fail for retry.
    // SAFETY: parent is live and leaf is NUL-terminated.
    let removed = unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), libc::AT_REMOVEDIR) };
    if removed == 0 {
        sync_parent(&parent)?;
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        sync_parent(&parent)?;
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
pub(crate) fn remove_directory_tree_no_follow(
    _root_path: &Path,
    _relative_path: &str,
    _max_entries: usize,
    _max_depth: usize,
) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure recursive directory cleanup is not implemented on this platform",
    ))
}

#[cfg(not(unix))]
pub(crate) fn remove_directory_tree_no_follow_at(
    _root_directory: &File,
    _relative_path: &str,
    _max_entries: usize,
    _max_depth: usize,
) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure descriptor-relative recursive cleanup is not implemented on this platform",
    ))
}

#[cfg(unix)]
fn remove_open_directory_contents_no_follow(
    directory: &File,
    remaining_entries: &mut usize,
    remaining_depth: usize,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    if remaining_depth == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed directory depth limit exceeded",
        ));
    }
    let dot = CString::new(".").expect("dot contains no NUL");
    let listing = open_at(
        directory.as_raw_fd(),
        &dot,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    let names = list_open_directory_names_bounded(listing, *remaining_entries)?;
    for name in names {
        *remaining_entries = remaining_entries.checked_sub(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "managed directory entry limit exceeded",
            )
        })?;
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "managed entry name contains NUL",
            )
        })?;
        match open_at(
            directory.as_raw_fd(),
            &name,
            libc::O_RDONLY
                | libc::O_NONBLOCK
                | libc::O_NOFOLLOW
                | libc::O_DIRECTORY
                | libc::O_CLOEXEC,
        ) {
            Ok(child) => {
                remove_open_directory_contents_no_follow(
                    &child,
                    remaining_entries,
                    remaining_depth - 1,
                )?;
                // SAFETY: directory is live and name is NUL-terminated.
                let removed = unsafe {
                    libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
                };
                if removed != 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::NotFound {
                        return Err(error);
                    }
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ENOTDIR)
                    || error.raw_os_error() == Some(libc::ELOOP) =>
            {
                if error.kind() == io::ErrorKind::NotFound {
                    continue;
                }
                // SAFETY: directory is live and name is NUL-terminated. unlinkat without
                // AT_REMOVEDIR removes the named non-directory or symlink without following it.
                let removed = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
                if removed != 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::NotFound {
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    directory.sync_all()
}

#[cfg(unix)]
fn open_relative_parent_no_follow(
    root_path: &Path,
    relative_path: &str,
) -> io::Result<(File, CString)> {
    use std::os::fd::AsRawFd;

    let segments = relative_path_segments(relative_path)?;
    let mut directory = open_path(
        root_path,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    for segment in &segments[..segments.len() - 1] {
        directory = open_at(
            directory.as_raw_fd(),
            segment,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )?;
    }
    Ok((
        directory,
        segments.last().expect("segments is not empty").clone(),
    ))
}

#[cfg(unix)]
fn relative_path_segments(relative_path: &str) -> io::Result<Vec<CString>> {
    use std::os::unix::ffi::OsStrExt;

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

    Ok(segments)
}

#[cfg(unix)]
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

#[cfg(unix)]
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

    #[test]
    fn m4a_media_uses_an_audio_content_type() {
        assert_eq!("audio/mp4", content_type(Path::new("episode.m4a")));
    }

    #[test]
    fn deletion_lock_keys_canonicalize_equivalent_item_paths() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        fs::create_dir_all(root_path.join("Movies")).expect("cache root should be created");
        let root_path = root_path
            .canonicalize()
            .expect("cache root should canonicalize");
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path: root_path.clone(),
            ..CacheServerOptions::default()
        }));
        let canonical = create_item_id("Movies/Sample.mp4");
        let alias = create_item_id("Movies//Sample.mp4");

        assert_ne!(canonical, alias);
        assert_eq!(
            Some((canonical.clone(), "Movies/Sample.mp4".to_owned())),
            library
                .canonical_deletable_item_id_blocking(&alias)
                .expect("equivalent item path should validate")
        );
        fs::remove_dir_all(root_path.join("Movies"))
            .expect("external directory removal should succeed");
        assert_eq!(
            Some((canonical, "Movies/Sample.mp4".to_owned())),
            library
                .canonical_deletable_item_id_blocking(&alias)
                .expect("missing parent should still identify the logical item")
        );
    }

    #[cfg(unix)]
    #[test]
    fn deletion_lock_keys_reject_an_existing_symlink_parent() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let outside_path = temp.path().join("outside");
        fs::create_dir_all(&root_path).expect("cache root should be created");
        fs::create_dir_all(&outside_path).expect("outside directory should be created");
        symlink(&outside_path, root_path.join("Movies")).expect("symlink should be created");
        let root_path = root_path
            .canonicalize()
            .expect("cache root should canonicalize");
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path,
            ..CacheServerOptions::default()
        }));

        assert_eq!(
            None,
            library
                .canonical_deletable_item_id_blocking(&create_item_id("Movies/Sample.mp4"))
                .expect("symlink parent should be rejected")
        );
    }

    #[test]
    fn media_paths_map_to_validated_item_ids() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let movie_dir = root_path.join("Movies");
        fs::create_dir_all(&movie_dir).unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let movie_dir = root_path.join("Movies");
        let movie_path = movie_dir.join("Sample.mp4");
        fs::write(&movie_path, b"sample").unwrap();
        let ignored_path = movie_dir.join("Sample.txt");
        fs::write(&ignored_path, b"sample").unwrap();

        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path: root_path.clone(),
            ..CacheServerOptions::default()
        }));

        let item_id = library
            .item_id_for_media_path_blocking(&movie_path)
            .expect("mp4 path should map to an item id");
        assert_eq!(
            PathBuf::from("Movies/Sample.mp4"),
            decode_item_id(&item_id).unwrap()
        );
        assert!(
            library
                .item_id_for_media_path_blocking(&ignored_path)
                .is_none()
        );
        assert!(
            library
                .item_id_for_media_path_blocking(&temp.path().join("Outside.mp4"))
                .is_none()
        );

        let escaped_dir = root_path.join("Bilibili");
        fs::create_dir_all(&escaped_dir).unwrap();
        let escaped_target = root_path.parent().unwrap().join("Escaped.mp4");
        fs::write(&escaped_target, b"sample").unwrap();
        let escaped_path = escaped_dir.join("../../Escaped.mp4");
        assert!(
            library
                .item_id_for_media_path_blocking(&escaped_path)
                .is_none()
        );
    }

    #[test]
    fn local_scan_excludes_all_internal_cache_files() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        fs::create_dir_all(root_path.join(".tvos-net-player/hls/session-1")).unwrap();
        fs::create_dir_all(root_path.join(".tvos-net-player/resources/resource-1")).unwrap();
        fs::write(
            root_path.join(".tvos-net-player/hls/session-1/video.m4s"),
            b"hls",
        )
        .unwrap();
        fs::write(
            root_path.join(".tvos-net-player/resources/resource-1/body.m4s"),
            b"resource",
        )
        .unwrap();
        fs::write(root_path.join("Visible.m4s"), b"media").unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path: root_path.clone(),
            allowed_extensions: vec![".m4s".to_owned()],
            ..CacheServerOptions::default()
        }));

        let page = library.list_items_page_blocking(None, 0, 50, BlockingCancellation::default());

        assert_eq!(1, page.items.len());
        assert_eq!("Visible.m4s", page.items[0].subtitle);
    }

    #[test]
    fn local_direct_lookup_excludes_all_internal_cache_files() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let internal_path = root_path.join(".tvos-net-player/resources/resource-1/body.m4s");
        fs::create_dir_all(internal_path.parent().unwrap()).unwrap();
        fs::write(&internal_path, b"resource").unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let internal_path = root_path.join(".tvos-net-player/resources/resource-1/body.m4s");
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path,
            allowed_extensions: vec![".m4s".to_owned()],
            ..CacheServerOptions::default()
        }));
        let item_id = create_item_id(".tvos-net-player/resources/resource-1/body.m4s");

        assert!(library.get_item_blocking(&item_id).is_none());
        assert!(
            library
                .get_media_file_blocking(&item_id, VARIANT_ID)
                .is_none()
        );
        assert!(
            library
                .item_id_for_media_path_blocking(&internal_path)
                .is_none()
        );
    }

    #[test]
    fn local_operations_reject_case_aliased_internal_cache_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let internal_relative_path = ".TVOS-NET-PLAYER/resources/resource-1/body.m4s";
        let visible_relative_path = ".TVOS-NET-PLAYER-backup/visible.m4s";
        let internal_path = root_path.join(internal_relative_path);
        let visible_path = root_path.join(visible_relative_path);
        fs::create_dir_all(internal_path.parent().unwrap()).unwrap();
        fs::create_dir_all(visible_path.parent().unwrap()).unwrap();
        fs::write(&internal_path, b"resource").unwrap();
        fs::write(&visible_path, b"visible").unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let internal_path = root_path.join(internal_relative_path);
        let visible_path = root_path.join(visible_relative_path);
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path,
            allowed_extensions: vec![".m4s".to_owned()],
            ..CacheServerOptions::default()
        }));

        let page = library.list_items_page_blocking(None, 0, 50, BlockingCancellation::default());
        assert_eq!(
            vec![visible_relative_path],
            page.items
                .iter()
                .map(|item| item.subtitle.as_str())
                .collect::<Vec<_>>()
        );

        let internal_item_id = create_item_id(internal_relative_path);
        assert!(library.get_item_blocking(&internal_item_id).is_none());
        assert!(
            library
                .get_media_file_blocking(&internal_item_id, VARIANT_ID)
                .is_none()
        );
        assert!(
            library
                .open_media_file_blocking(&internal_item_id, VARIANT_ID)
                .is_none()
        );
        assert!(
            library
                .item_id_for_media_path_blocking(&internal_path)
                .is_none()
        );
        assert!(
            !library
                .delete_item_blocking(&internal_item_id)
                .expect("case-aliased internal cache delete should be ignored")
        );
        assert!(internal_path.exists());

        let visible_item_id = library
            .item_id_for_media_path_blocking(&visible_path)
            .expect("a longer first component should remain visible");
        assert!(
            library
                .open_media_file_blocking(&visible_item_id, VARIANT_ID)
                .is_some()
        );
        assert!(
            library
                .delete_item_blocking(&visible_item_id)
                .expect("visible media delete should succeed")
        );
        assert!(!visible_path.exists());
    }

    #[test]
    fn delete_item_removes_local_media_file() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let movie_dir = root_path.join("Movies");
        fs::create_dir_all(&movie_dir).unwrap();
        let movie_path = movie_dir.join("Sample.mp4");
        fs::write(&movie_path, b"sample").unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let movie_path = root_path.join("Movies/Sample.mp4");
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path,
            ..CacheServerOptions::default()
        }));
        let item_id = library
            .item_id_for_media_path_blocking(&movie_path)
            .expect("movie path should resolve to an item id");

        assert!(
            library
                .delete_item_blocking(&item_id)
                .expect("delete should succeed")
        );
        assert!(!movie_path.exists());
        assert!(
            !library
                .delete_item_blocking(&item_id)
                .expect("second delete should be idempotent")
        );
    }

    #[tokio::test]
    async fn publication_lease_blocks_deletion_until_the_artifact_is_committed() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        fs::create_dir_all(&root_path).expect("cache root should be created");
        let root_path = root_path
            .canonicalize()
            .expect("cache root should canonicalize");
        let media_path = root_path.join("leased.mp4");
        fs::write(&media_path, b"leased media").expect("media should be written");
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path,
            ..CacheServerOptions::default()
        }));
        let lease = library
            .reserve_media_path_for_publication(media_path.clone())
            .await
            .expect("media should be reservable for publication");
        let second_lease = library
            .reserve_media_path_for_publication(media_path.clone())
            .await
            .expect("the same media should allow concurrent publication leases");
        let item_id = lease.item_id.clone();
        let deleting_library = library.clone();
        let deletion = tokio::spawn(async move { deleting_library.delete_item(&item_id).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !deletion.is_finished(),
            "deletion must wait for the publication lease"
        );
        drop(lease);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !deletion.is_finished(),
            "deletion must wait for every publication lease"
        );
        drop(second_lease);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), deletion)
                .await
                .expect("deletion should finish after publication")
                .expect("deletion task should not panic")
                .expect("deletion should succeed")
        );
        assert!(!media_path.exists());
    }

    #[test]
    fn owned_output_gate_blocks_library_reads_until_durable_release() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let media_path = root_path.join("Bilibili/task-1/video.mp4");
        fs::create_dir_all(media_path.parent().unwrap())
            .expect("media directory should be created");
        fs::write(&media_path, b"video").expect("media should be written");
        let root_path = root_path
            .canonicalize()
            .expect("cache root should canonicalize");
        let media_path = root_path.join("Bilibili/task-1/video.mp4");
        let gate = Arc::new(
            LibraryPublicationGate::unknown_for_output_directory(
                &root_path,
                &root_path.join("Bilibili"),
            )
            .expect("output directory should be valid"),
        );
        let library = LocalMediaLibrary::new_with_publication_gate(
            Arc::new(CacheServerOptions {
                root_path,
                allow_library_item_delete: true,
                ..CacheServerOptions::default()
            }),
            Arc::clone(&gate),
        );
        let item_id = library
            .item_id_for_media_path_blocking(&media_path)
            .expect("internal publication probing should bypass the read gate");

        let assert_hidden = || {
            assert!(
                library
                    .list_items_page_blocking(None, 0, 50, BlockingCancellation::default())
                    .items
                    .is_empty()
            );
            assert_eq!(
                0,
                library.count_items_blocking(BlockingCancellation::default())
            );
            assert!(library.get_item_blocking(&item_id).is_none());
            assert!(
                library
                    .get_media_file_blocking(&item_id, VARIANT_ID)
                    .is_none()
            );
            assert!(
                library
                    .open_media_file_blocking(&item_id, VARIANT_ID)
                    .is_none()
            );
            assert!(!library.delete_item_blocking(&item_id).unwrap());
            assert!(media_path.exists());
        };
        assert_hidden();

        gate.install_durable_blocked_paths(["Bilibili/task-1"])
            .expect("durable task ownership should install");
        assert_hidden();

        gate.install_durable_blocked_paths(std::iter::empty())
            .expect("durable terminal publication should release the gate");
        assert_eq!(
            1,
            library.count_items_blocking(BlockingCancellation::default())
        );
        assert!(library.get_item_blocking(&item_id).is_some());
        assert!(
            library
                .get_media_file_blocking(&item_id, VARIANT_ID)
                .is_some()
        );
        assert!(
            library
                .open_media_file_blocking(&item_id, VARIANT_ID)
                .is_some()
        );
    }

    #[test]
    fn filesystem_aliases_share_canonical_gate_and_mutation_identity() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let media_path = root_path.join("Bilibili/Task-One/Video.mp4");
        fs::create_dir_all(media_path.parent().unwrap())
            .expect("media directory should be created");
        fs::write(&media_path, b"video").expect("media should be written");
        let root_path = root_path
            .canonicalize()
            .expect("cache root should canonicalize");
        let alias_path = root_path.join("bilibili/task-one/video.mp4");
        if !alias_path.is_file() {
            return;
        }

        let gate = Arc::new(LibraryPublicationGate::known_empty());
        gate.install_durable_blocked_paths(["Bilibili/Task-One"])
            .expect("durable task ownership should install");
        let library = LocalMediaLibrary::new_with_publication_gate(
            Arc::new(CacheServerOptions {
                root_path,
                allow_library_item_delete: true,
                ..CacheServerOptions::default()
            }),
            Arc::clone(&gate),
        );
        let alias_id = create_item_id("bilibili/task-one/video.mp4");
        assert!(library.get_item_blocking(&alias_id).is_none());
        assert!(
            library
                .canonical_deletable_item_id_blocking(&alias_id)
                .expect("alias validation should succeed")
                .is_none()
        );

        gate.install_durable_blocked_paths(std::iter::empty::<&str>())
            .expect("durable publication should release the gate");
        let canonical_id = create_item_id("Bilibili/Task-One/Video.mp4");
        assert_eq!(
            Some(canonical_id.clone()),
            library.item_id_for_media_path_blocking(&alias_path)
        );
        assert_eq!(
            Some((canonical_id, "Bilibili/Task-One/Video.mp4".to_owned())),
            library
                .canonical_deletable_item_id_blocking(&alias_id)
                .expect("alias validation should succeed")
        );
    }

    #[test]
    fn root_output_gate_blocks_the_entire_library_until_durable_state_is_loaded() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        fs::create_dir_all(&root_path).expect("cache root should be created");
        fs::write(root_path.join("video.mp4"), b"video").expect("media should be written");
        let root_path = root_path
            .canonicalize()
            .expect("cache root should canonicalize");
        let gate = Arc::new(
            LibraryPublicationGate::unknown_for_output_directory(&root_path, &root_path)
                .expect("the cache root should be a valid output directory"),
        );
        let library = LocalMediaLibrary::new_with_publication_gate(
            Arc::new(CacheServerOptions {
                root_path,
                ..CacheServerOptions::default()
            }),
            Arc::clone(&gate),
        );

        assert!(
            library
                .list_items_page_blocking(None, 0, 50, BlockingCancellation::default())
                .items
                .is_empty()
        );

        gate.install_durable_blocked_paths(std::iter::empty::<&str>())
            .expect("an empty durable block set should release the library");
        assert_eq!(
            1,
            library
                .list_items_page_blocking(None, 0, 50, BlockingCancellation::default())
                .items
                .len()
        );
    }

    #[test]
    fn list_and_count_retry_when_publication_is_released_after_candidate_scan() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let media_path = root_path.join("Bilibili/task-1/video.mp4");
        fs::create_dir_all(media_path.parent().unwrap())
            .expect("media directory should be created");
        fs::write(&media_path, b"video").expect("media should be written");
        let root_path = root_path
            .canonicalize()
            .expect("cache root should canonicalize");
        let gate = Arc::new(LibraryPublicationGate::known_empty());
        gate.install_durable_blocked_paths(["Bilibili/task-1"])
            .expect("task output should start blocked");
        let library = LocalMediaLibrary::new_with_publication_gate(
            Arc::new(CacheServerOptions {
                root_path,
                ..CacheServerOptions::default()
            }),
            Arc::clone(&gate),
        );

        let mut list_scans = 0;
        let page = library.list_items_page_blocking_with_scan_observer(
            None,
            0,
            50,
            BlockingCancellation::default(),
            || {
                list_scans += 1;
                if list_scans == 1 {
                    gate.install_durable_blocked_paths(std::iter::empty::<&str>())
                        .expect("publication should be released after the first list scan");
                }
            },
        );
        assert_eq!(2, list_scans);
        assert_eq!(1, page.items.len());

        gate.install_durable_blocked_paths(["Bilibili/task-1"])
            .expect("task output should be blocked again");
        let mut count_scans = 0;
        let count = library.count_items_blocking_with_scan_observer(
            BlockingCancellation::default(),
            || {
                count_scans += 1;
                if count_scans == 1 {
                    gate.install_durable_blocked_paths(std::iter::empty::<&str>())
                        .expect("publication should be released after the first count scan");
                }
            },
        );
        assert_eq!(2, count_scans);
        assert_eq!(1, count);
    }

    #[test]
    fn owned_output_gate_update_does_not_wait_for_an_in_flight_snapshot() {
        let gate = Arc::new(LibraryPublicationGate::known_empty());
        let snapshot = gate.snapshot();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        let updating_gate = Arc::clone(&gate);
        let update = std::thread::spawn(move || {
            updating_gate
                .install_durable_blocked_paths(["Bilibili/task-1"])
                .expect("gate update should succeed");
            completed_tx
                .send(())
                .expect("completion signal should be delivered");
        });

        completed_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a gate update must not wait for a filesystem scan using an older snapshot");
        update.join().expect("gate update should finish");
        assert!(!snapshot.blocks(Path::new("Bilibili/task-1/video.mp4")));
        assert!(
            gate.snapshot()
                .blocks(Path::new("Bilibili/task-1/video.mp4"))
        );
    }

    #[test]
    fn stable_snapshot_retries_when_ownership_is_registered_during_work() {
        let gate = Arc::new(LibraryPublicationGate::known_empty());
        let (snapshot_in_use_tx, snapshot_in_use_rx) = std::sync::mpsc::channel();
        let (updated_tx, updated_rx) = std::sync::mpsc::channel();
        let updating_gate = Arc::clone(&gate);
        let update = std::thread::spawn(move || {
            snapshot_in_use_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the first snapshot should enter filesystem work");
            updating_gate
                .install_durable_blocked_paths(["Bilibili/task-1"])
                .expect("gate update should succeed");
            updated_tx
                .send(())
                .expect("the gate update should be observable");
        });

        let mut attempts = 0;
        let visible = gate
            .with_stable_snapshot(|snapshot| {
                attempts += 1;
                if attempts == 1 {
                    snapshot_in_use_tx
                        .send(())
                        .expect("the updater should be notified");
                    updated_rx
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .expect("ownership should be registered during the first attempt");
                }
                !snapshot.blocks(Path::new("Bilibili/task-1/video.mp4"))
            })
            .expect("the publication view should stabilize");

        update.join().expect("gate update should finish");
        assert_eq!(2, attempts);
        assert!(!visible);
    }

    #[test]
    fn stable_snapshot_ignores_reinstalling_the_same_blocked_paths_during_work() {
        let gate = LibraryPublicationGate::known_empty();
        gate.install_durable_blocked_paths(["Bilibili/task-1"])
            .expect("initial ownership should install");
        let initial_snapshot = gate.snapshot();
        let mut attempts = 0;

        let blocked = gate
            .with_stable_snapshot(|snapshot| {
                attempts += 1;
                for _ in 0..=MAX_PUBLICATION_GATE_STABILIZATION_ATTEMPTS {
                    gate.install_durable_blocked_paths(["Bilibili/task-1", "Bilibili/task-1"])
                        .expect("equivalent ownership should reinstall idempotently");
                }
                snapshot.blocks(Path::new("Bilibili/task-1/video.mp4"))
            })
            .expect("equivalent durable state should not invalidate the scan");

        assert_eq!(1, attempts);
        assert!(blocked);
        assert!(Arc::ptr_eq(&initial_snapshot, &gate.snapshot()));
    }

    #[test]
    fn stale_snapshot_cannot_enter_an_irreversible_gate_section() {
        let gate = LibraryPublicationGate::known_empty();
        let stale_snapshot = gate.snapshot();
        gate.install_durable_blocked_paths(["Bilibili/task-1"])
            .expect("gate update should succeed");

        assert!(gate.read_if_current(&stale_snapshot).is_none());
        let current_snapshot = gate.snapshot();
        let current_guard = gate
            .read_if_current(&current_snapshot)
            .expect("the current snapshot should enter the guarded section");
        assert!(current_guard.blocks(Path::new("Bilibili/task-1/video.mp4")));
    }

    #[test]
    fn delete_item_errors_when_cache_root_disappears() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let movie_dir = root_path.join("Movies");
        fs::create_dir_all(&movie_dir).unwrap();
        let movie_path = movie_dir.join("Sample.mp4");
        fs::write(&movie_path, b"sample").unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let movie_path = root_path.join("Movies/Sample.mp4");
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path: root_path.clone(),
            ..CacheServerOptions::default()
        }));
        let item_id = library
            .item_id_for_media_path_blocking(&movie_path)
            .expect("movie path should resolve to an item id");
        fs::remove_dir_all(&root_path).expect("cache root should be removable");

        let error = library
            .delete_item_blocking(&item_id)
            .expect_err("missing cache root should be reported");

        assert_eq!(io::ErrorKind::NotFound, error.kind());
    }

    #[cfg(unix)]
    #[test]
    fn remove_file_no_follow_treats_a_missing_intermediate_directory_as_absent() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let movie_dir = root_path.join("Movies/Series");
        fs::create_dir_all(&movie_dir).unwrap();
        fs::write(movie_dir.join("Episode.mp4"), b"sample").unwrap();
        fs::remove_dir_all(root_path.join("Movies"))
            .expect("validated nested media directory should be removable");

        assert!(
            !remove_file_no_follow(&root_path, "Movies/Series/Episode.mp4")
                .expect("a raced nested deletion should remain idempotent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_cleanup_stays_bound_to_a_replaced_root() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let displaced_root_path = temp.path().join("displaced-cache");
        let file_relative_path = "Bilibili/task-file/video.mp4";
        let directory_relative_path = "Bilibili/task-directory";
        fs::create_dir_all(root_path.join("Bilibili/task-file")).unwrap();
        fs::create_dir_all(root_path.join(directory_relative_path)).unwrap();
        fs::write(root_path.join(file_relative_path), b"original-file").unwrap();
        fs::write(
            root_path.join(directory_relative_path).join("video.mp4"),
            b"original-directory",
        )
        .unwrap();
        let root_directory = open_path(
            &root_path,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
        .unwrap();

        fs::rename(&root_path, &displaced_root_path).unwrap();
        fs::create_dir_all(root_path.join("Bilibili/task-file")).unwrap();
        fs::create_dir_all(root_path.join(directory_relative_path)).unwrap();
        fs::write(root_path.join(file_relative_path), b"replacement-file").unwrap();
        fs::write(
            root_path.join(directory_relative_path).join("video.mp4"),
            b"replacement-directory",
        )
        .unwrap();

        assert!(remove_file_no_follow_at(&root_directory, file_relative_path).unwrap());
        assert!(
            remove_directory_tree_no_follow_at(&root_directory, directory_relative_path, 8, 4,)
                .unwrap()
        );

        assert!(!displaced_root_path.join(file_relative_path).exists());
        assert!(!displaced_root_path.join(directory_relative_path).exists());
        assert_eq!(
            b"replacement-file",
            fs::read(root_path.join(file_relative_path))
                .unwrap()
                .as_slice()
        );
        assert_eq!(
            b"replacement-directory",
            fs::read(root_path.join(directory_relative_path).join("video.mp4"))
                .unwrap()
                .as_slice()
        );
    }

    #[cfg(unix)]
    #[test]
    fn remove_file_no_follow_retries_parent_sync_after_unlink_sync_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let movie_dir = root_path.join("Movies");
        let movie_path = movie_dir.join("Sample.mp4");
        fs::create_dir_all(&movie_dir).unwrap();
        fs::write(&movie_path, b"sample").unwrap();
        let mut failed_syncs = 0;

        let error =
            remove_entry_no_follow_with_parent_sync(&root_path, "Movies/Sample.mp4", 0, |_| {
                failed_syncs += 1;
                Err(io::Error::other("injected parent directory sync failure"))
            })
            .expect_err("an unsynchronized unlink must remain retryable");

        assert_eq!(io::ErrorKind::Other, error.kind());
        assert_eq!(1, failed_syncs);
        assert!(!movie_path.exists());

        let mut retry_syncs = 0;
        let removed = remove_entry_no_follow_with_parent_sync(
            &root_path,
            "Movies/Sample.mp4",
            0,
            |directory| {
                retry_syncs += 1;
                directory.sync_all()
            },
        )
        .expect("the missing entry should retry its parent directory sync");

        assert!(!removed);
        assert_eq!(1, retry_syncs);
    }

    #[test]
    fn delete_item_rejects_all_internal_cache_files() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let internal_path = root_path.join(".tvos-net-player/resources/resource-1/body.m4s");
        fs::create_dir_all(internal_path.parent().unwrap()).unwrap();
        fs::write(&internal_path, b"resource").unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let internal_path = root_path.join(".tvos-net-player/resources/resource-1/body.m4s");
        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path,
            allowed_extensions: vec![".m4s".to_owned()],
            ..CacheServerOptions::default()
        }));
        let item_id = create_item_id(".tvos-net-player/resources/resource-1/body.m4s");

        assert!(
            !library
                .delete_item_blocking(&item_id)
                .expect("internal cache delete should be ignored")
        );
        assert!(internal_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_directory_no_follow_removes_empty_leaf_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let resource_dir = root_path.join(".tvos-net-player/resources/resource-1");
        fs::create_dir_all(&resource_dir).unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let resource_dir = root_path.join(".tvos-net-player/resources/resource-1");

        remove_empty_directory_no_follow(&root_path, ".tvos-net-player/resources/resource-1")
            .expect("empty resource directory should be removed");

        assert!(!resource_dir.exists());
        assert!(root_path.join(".tvos-net-player/resources").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_directory_no_follow_retries_parent_sync_after_unlink_sync_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let resource_dir = root_path.join(".tvos-net-player/resources/resource-1");
        fs::create_dir_all(&resource_dir).unwrap();
        let mut failed_syncs = 0;

        remove_entry_no_follow_with_parent_sync(
            &root_path,
            ".tvos-net-player/resources/resource-1",
            libc::AT_REMOVEDIR,
            |_| {
                failed_syncs += 1;
                Err(io::Error::other("injected parent directory sync failure"))
            },
        )
        .expect_err("an unsynchronized directory unlink must remain retryable");

        assert_eq!(1, failed_syncs);
        assert!(!resource_dir.exists());

        let mut retry_syncs = 0;
        let removed = remove_entry_no_follow_with_parent_sync(
            &root_path,
            ".tvos-net-player/resources/resource-1",
            libc::AT_REMOVEDIR,
            |directory| {
                retry_syncs += 1;
                directory.sync_all()
            },
        )
        .expect("the missing directory should retry its parent directory sync");

        assert!(!removed);
        assert_eq!(1, retry_syncs);
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_directory_no_follow_treats_a_missing_intermediate_directory_as_absent() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let resources_dir = root_path.join(".tvos-net-player/resources");
        fs::create_dir_all(resources_dir.join("resource-1")).unwrap();
        fs::remove_dir_all(&resources_dir)
            .expect("validated resource namespace should be removable");
        let root_path = root_path.canonicalize().unwrap();

        assert!(
            !remove_empty_directory_no_follow(&root_path, ".tvos-net-player/resources/resource-1")
                .expect("a raced namespace deletion should remain idempotent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_directory_no_follow_rejects_non_empty_leaf_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let resource_dir = root_path.join(".tvos-net-player/resources/resource-1");
        fs::create_dir_all(&resource_dir).unwrap();
        fs::write(resource_dir.join("body"), b"resource").unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let resource_dir = root_path.join(".tvos-net-player/resources/resource-1");

        let error =
            remove_empty_directory_no_follow(&root_path, ".tvos-net-player/resources/resource-1")
                .expect_err("non-empty resource directory must not be removed");

        assert_eq!(io::ErrorKind::DirectoryNotEmpty, error.kind());
        assert!(resource_dir.is_dir());
        assert!(resource_dir.join("body").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_directory_no_follow_refuses_symlink_leaf() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let resources_dir = root_path.join(".tvos-net-player/resources");
        let outside_dir = temp.path().join("outside-resource");
        fs::create_dir_all(&resources_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        symlink(&outside_dir, resources_dir.join("resource-1")).unwrap();
        let root_path = root_path.canonicalize().unwrap();
        let link_path = root_path.join(".tvos-net-player/resources/resource-1");

        remove_empty_directory_no_follow(&root_path, ".tvos-net-player/resources/resource-1")
            .expect_err("symlink leaf must not be followed or removed");

        assert!(outside_dir.is_dir());
        assert!(
            fs::symlink_metadata(link_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_directory_no_follow_refuses_symlink_parent() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let internal_dir = root_path.join(".tvos-net-player");
        let outside_resources = temp.path().join("outside-resources");
        let outside_resource_dir = outside_resources.join("resource-1");
        fs::create_dir_all(&internal_dir).unwrap();
        fs::create_dir_all(&outside_resource_dir).unwrap();
        symlink(&outside_resources, internal_dir.join("resources")).unwrap();
        let root_path = root_path.canonicalize().unwrap();

        remove_empty_directory_no_follow(&root_path, ".tvos-net-player/resources/resource-1")
            .expect_err("symlink parent must not be followed");

        assert!(outside_resource_dir.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_directory_no_follow_rejects_non_normal_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        fs::create_dir_all(&root_path).unwrap();
        let root_path = root_path.canonicalize().unwrap();

        for relative_path in [
            "",
            ".",
            "../resource-1",
            ".tvos-net-player/../resource-1",
            "/resource-1",
        ] {
            let error = remove_empty_directory_no_follow(&root_path, relative_path)
                .expect_err("non-normal directory path should be rejected");
            assert_eq!(io::ErrorKind::InvalidInput, error.kind());
        }
    }

    #[cfg(unix)]
    #[test]
    fn task_output_directory_creation_and_no_replace_rename_are_contained() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        fs::create_dir_all(&root_path).unwrap();
        let root_path = root_path.canonicalize().unwrap();

        ensure_directory_no_follow(&root_path, "Bilibili").unwrap();
        create_directory_exclusive_no_follow(&root_path, ".tvos-net-player/bbdown-staging/task-1")
            .unwrap();
        fs::write(
            root_path.join(".tvos-net-player/bbdown-staging/task-1/video.mp4"),
            b"video",
        )
        .unwrap();

        rename_directory_no_replace_no_follow(
            &root_path,
            ".tvos-net-player/bbdown-staging/task-1",
            "Bilibili/task-1",
            16,
            8,
        )
        .unwrap();

        assert!(
            !root_path
                .join(".tvos-net-player/bbdown-staging/task-1")
                .exists()
        );
        assert_eq!(
            b"video",
            fs::read(root_path.join("Bilibili/task-1/video.mp4"))
                .unwrap()
                .as_slice()
        );
        assert_eq!(
            io::ErrorKind::AlreadyExists,
            create_directory_exclusive_no_follow(&root_path, "Bilibili/task-1")
                .unwrap_err()
                .kind()
        );
    }

    #[cfg(unix)]
    #[test]
    fn task_output_directory_publication_rejects_symlinks_and_entry_overflow() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let staging = root_path.join(".tvos-net-player/bbdown-staging/task-1");
        let outside = temp.path().join("outside.mp4");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(root_path.join("Bilibili")).unwrap();
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, staging.join("video.mp4")).unwrap();
        let root_path = root_path.canonicalize().unwrap();

        rename_directory_no_replace_no_follow(
            &root_path,
            ".tvos-net-player/bbdown-staging/task-1",
            "Bilibili/task-1",
            16,
            8,
        )
        .expect_err("a symlink cannot be synchronized for publication");
        assert!(staging.exists());
        assert!(!root_path.join("Bilibili/task-1").exists());
        assert_eq!(b"outside", fs::read(&outside).unwrap().as_slice());

        fs::remove_file(staging.join("video.mp4")).unwrap();
        fs::write(staging.join("video.mp4"), b"video").unwrap();
        fs::write(staging.join("audio.m4a"), b"audio").unwrap();
        let error = rename_directory_no_replace_no_follow(
            &root_path,
            ".tvos-net-player/bbdown-staging/task-1",
            "Bilibili/task-1",
            1,
            8,
        )
        .expect_err("publication must enforce the entry bound before rename");
        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert!(staging.exists());
        assert!(!root_path.join("Bilibili/task-1").exists());
    }

    #[cfg(unix)]
    #[test]
    fn recursive_directory_cleanup_unlinks_children_without_following_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let owned = root_path.join(".tvos-net-player/bbdown-staging/task-1");
        let outside = temp.path().join("outside");
        fs::create_dir_all(owned.join("nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(owned.join("nested/video.mp4"), b"video").unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        symlink(&outside, owned.join("outside-link")).unwrap();
        let root_path = root_path.canonicalize().unwrap();

        assert!(
            remove_directory_tree_no_follow(
                &root_path,
                ".tvos-net-player/bbdown-staging/task-1",
                16,
                8,
            )
            .unwrap()
        );

        assert!(!owned.exists());
        assert_eq!(
            b"keep",
            fs::read(outside.join("keep.txt")).unwrap().as_slice()
        );
        assert!(
            !remove_directory_tree_no_follow(
                &root_path,
                ".tvos-net-player/bbdown-staging/task-1",
                16,
                8,
            )
            .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn recursive_directory_cleanup_syncs_the_nearest_parent_of_a_missing_ancestor() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let internal_root = root_path.join(".tvos-net-player");
        fs::create_dir_all(&internal_root).unwrap();
        let expected_parent = fs::metadata(&internal_root).unwrap();
        let mut parent_syncs = 0;

        let removed = remove_directory_tree_no_follow_with_parent_sync(
            &root_path,
            ".tvos-net-player/bbdown-staging/task-1",
            16,
            8,
            |parent| {
                parent_syncs += 1;
                let actual_parent = parent.metadata()?;
                assert_eq!(expected_parent.dev(), actual_parent.dev());
                assert_eq!(expected_parent.ino(), actual_parent.ino());
                parent.sync_all()
            },
        )
        .expect("a durably missing ancestor should make the owned tree absent");

        assert!(!removed);
        assert_eq!(1, parent_syncs);
    }

    #[cfg(unix)]
    #[test]
    fn recursive_directory_cleanup_retries_parent_sync_after_unlink_sync_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let owned = root_path.join(".tvos-net-player/bbdown-staging/task-1");
        fs::create_dir_all(&owned).unwrap();
        let mut failed_syncs = 0;

        remove_directory_tree_no_follow_with_parent_sync(
            &root_path,
            ".tvos-net-player/bbdown-staging/task-1",
            16,
            8,
            |_| {
                failed_syncs += 1;
                Err(io::Error::other("injected parent directory sync failure"))
            },
        )
        .expect_err("an unsynchronized tree unlink must remain retryable");

        assert_eq!(1, failed_syncs);
        assert!(!owned.exists());

        let mut retry_syncs = 0;
        let removed = remove_directory_tree_no_follow_with_parent_sync(
            &root_path,
            ".tvos-net-player/bbdown-staging/task-1",
            16,
            8,
            |parent| {
                retry_syncs += 1;
                parent.sync_all()
            },
        )
        .expect("the missing tree should retry its containing-directory sync");

        assert!(!removed);
        assert_eq!(1, retry_syncs);
    }

    #[cfg(unix)]
    #[test]
    fn recursive_directory_cleanup_refuses_a_symlinked_owned_root() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let staging_parent = root_path.join(".tvos-net-player/bbdown-staging");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&staging_parent).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        symlink(&outside, staging_parent.join("task-1")).unwrap();
        let root_path = root_path.canonicalize().unwrap();

        remove_directory_tree_no_follow(
            &root_path,
            ".tvos-net-player/bbdown-staging/task-1",
            16,
            8,
        )
        .expect_err("a symlinked owned root is an access-policy change");

        assert_eq!(
            b"keep",
            fs::read(outside.join("keep.txt")).unwrap().as_slice()
        );
        assert!(
            fs::symlink_metadata(staging_parent.join("task-1"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_directory_names_no_follow_bounded_fails_when_limit_is_exceeded() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache");
        let resources_dir = root_path.join(".tvos-net-player/resources");
        fs::create_dir_all(resources_dir.join("resource-a")).unwrap();
        fs::create_dir_all(resources_dir.join("resource-b")).unwrap();
        let root_path = root_path.canonicalize().unwrap();

        let error =
            list_directory_names_no_follow_bounded(&root_path, ".tvos-net-player/resources", 1)
                .expect_err("directory listing should fail after the configured limit");

        assert_eq!(io::ErrorKind::InvalidData, error.kind());

        let mut names =
            list_directory_names_no_follow_bounded(&root_path, ".tvos-net-player/resources", 2)
                .expect("directory listing should fit within the configured limit");
        names.sort();
        assert_eq!(
            vec!["resource-a".to_owned(), "resource-b".to_owned()],
            names
        );
    }

    #[test]
    fn cancelled_scan_returns_interrupted() {
        let cancellation = BlockingCancellation::default();
        let guard = cancellation.guard();
        drop(guard);

        let mut candidates = Vec::new();
        let publication_gate = LibraryPublicationGateState::Known {
            blocked_paths: HashSet::new(),
        };
        let result = collect_media_candidates(
            Path::new("."),
            Path::new("."),
            &[".mp4".to_owned()],
            &None,
            &cancellation,
            &publication_gate,
            &mut candidates,
        );

        assert_eq!(io::ErrorKind::Interrupted, result.unwrap_err().kind());
    }

    #[cfg(unix)]
    #[test]
    fn root_availability_rejects_symlink_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_parent = temp.path().join("real-parent");
        let real_root = real_parent.join("cache");
        fs::create_dir_all(&real_root).unwrap();
        let link_parent = temp.path().join("link-parent");
        symlink(&real_parent, &link_parent).unwrap();

        let library = LocalMediaLibrary::new(Arc::new(CacheServerOptions {
            root_path: link_parent.join("cache"),
            ..CacheServerOptions::default()
        }));

        assert!(!library.is_root_available_blocking());
        assert!(path_contains_link(
            &library.root_path(),
            &library.root_path().join("Movie.mp4")
        ));
    }
}
