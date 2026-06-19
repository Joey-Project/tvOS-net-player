mod bbdown_adapter;
mod bilibili_playback;
pub mod bilibili_worker;
mod bonjour;
pub mod config;
pub mod generated;
pub mod grpc_services;
mod hls;
mod hls_cache;
mod hls_fill_scheduler;
pub mod library;
pub mod media;
pub mod playback;
pub mod task_registry;
mod task_store;

use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use axum::{Router, routing::get};
use bbdown_adapter::BbdownBilibiliAdapter;
use bilibili_worker::{BilibiliDownloadAdapter, run_bilibili_task_worker};
use generated::tvos_net_player::v1::{
    LibraryItem, PlaybackProtocol, PlaybackSource, TaskKind, TaskState,
    cache_service_server::CacheServiceServer, library_service_server::LibraryServiceServer,
    server_service_server::ServerServiceServer, task_service_server::TaskServiceServer,
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Status, transport::Server};

use crate::{
    bilibili_playback::BilibiliPlaybackPlanner,
    config::CacheServerOptions,
    grpc_services::{
        CacheGrpcService, HlsCacheFinalizationFailureMode, LibraryGrpcService, ServerGrpcService,
        TaskGrpcService,
    },
    hls::{HlsPlaybackRegistry, HlsPlaybackSession},
    hls_cache::{
        HlsCacheCompletedEntry, HlsCacheEvictionPolicy, HlsCacheEvictionSummary,
        HlsCacheStatusSnapshot, HlsCacheStore, sanitized_completed_session,
    },
    hls_fill_scheduler::HlsFillScheduler,
    library::LocalMediaLibrary,
    media::{
        MediaState, hls_master_playlist_get, hls_master_playlist_head, hls_segment_get,
        hls_segment_head, media_get, media_head,
    },
    playback::PlaybackUriFactory,
    task_registry::BilibiliTaskRegistry,
};

const BBDOWN_WORKER_MAX_CONCURRENT_TASKS: usize = 1;
const HLS_CACHE_FINALIZATION_MAX_CONCURRENT_TASKS: usize = 1;
const HLS_UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HLS_UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(20);
const HLS_UPSTREAM_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const HLS_CACHE_EVICTION_CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);
const HLS_CACHE_PLAYBACK_LEASE_DURATION: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub struct AppState {
    pub options: Arc<CacheServerOptions>,
    pub library: Arc<LocalMediaLibrary>,
    pub playback_uri_factory: Arc<PlaybackUriFactory>,
    pub tasks: Arc<BilibiliTaskRegistry>,
    pub(crate) hls_sessions: HlsPlaybackRegistry,
    pub(crate) hls_cache: HlsCacheStore,
    pub(crate) hls_upstream_client: reqwest::Client,
    pub(crate) playback_planner: Arc<dyn BilibiliPlaybackPlanner>,
    pub(crate) playback_planning_permits: Arc<Semaphore>,
    pub(crate) hls_cache_finalization_permits: Arc<Semaphore>,
    pub(crate) hls_fill_scheduler: HlsFillScheduler,
    pub(crate) completed_hls_cache_playback_supported: bool,
    pub(crate) last_hls_cache_eviction: Arc<Mutex<Option<HlsCacheEvictionSummary>>>,
    hls_cache_quota_enforcement_lock: Arc<Mutex<()>>,
    hls_cache_eviction_protected_session_ids: Arc<Mutex<HashMap<String, usize>>>,
    hls_cache_playback_leases: Arc<Mutex<HashMap<String, SystemTime>>>,
}

pub(crate) struct HlsCacheEvictionProtectionGuard {
    session_id: String,
    protected_session_ids: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for HlsCacheEvictionProtectionGuard {
    fn drop(&mut self) {
        let mut protected_session_ids = self
            .protected_session_ids
            .lock()
            .expect("HLS cache eviction protection lock poisoned");
        let Some(count) = protected_session_ids.get_mut(&self.session_id) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            protected_session_ids.remove(&self.session_id);
        }
    }
}

impl AppState {
    pub fn new(options: CacheServerOptions) -> Self {
        Self::new_with_playback_planner_factory(options, |options, library| {
            Arc::new(BbdownBilibiliAdapter::new(options, library))
        })
    }

    #[cfg(test)]
    pub(crate) fn new_with_playback_planner(
        options: CacheServerOptions,
        playback_planner: Arc<dyn BilibiliPlaybackPlanner>,
    ) -> Self {
        Self::new_with_playback_planner_factory(options, |_options, _library| playback_planner)
    }

    fn new_with_playback_planner_factory(
        options: CacheServerOptions,
        playback_planner_factory: impl FnOnce(
            Arc<CacheServerOptions>,
            Arc<LocalMediaLibrary>,
        ) -> Arc<dyn BilibiliPlaybackPlanner>,
    ) -> Self {
        options.validate().expect("invalid cache server options");
        let options = options.normalized_for_runtime();
        options.validate().expect("invalid cache server options");
        let task_state_path = options.task_state_path();
        let task_retention_policy = options.task_retention_policy();
        let options = Arc::new(options);
        let library = Arc::new(LocalMediaLibrary::new(Arc::clone(&options)));
        let playback_uri_factory = Arc::new(PlaybackUriFactory::new(Arc::clone(&options)));
        let tasks = Arc::new(BilibiliTaskRegistry::with_persistence_path_and_retention(
            task_state_path,
            task_retention_policy,
        ));
        let hls_sessions = HlsPlaybackRegistry::default();
        let hls_cache = HlsCacheStore::new(library.root_path());
        let (mut restored_hls_sessions, hls_cache_scan_succeeded) = match hls_cache.load_sessions()
        {
            Ok(sessions) => (sessions, true),
            Err(error) => {
                eprintln!(
                    "Failed to scan HLS cache sessions during startup; preserving persisted playback tasks until the cache root is readable: {error}"
                );
                (Vec::new(), false)
            }
        };
        let restorable_playback_session_ids = restored_hls_sessions
            .iter()
            .map(|session| session.id.clone())
            .collect();
        let completed_hls_cache_playback_supported = library.supports_http_range_playback();
        let completed_cache_session_ids = if completed_hls_cache_playback_supported {
            hls_cache.completed_session_ids(&restored_hls_sessions)
        } else {
            HashSet::new()
        };
        let restorable_completed_session_ids = completed_cache_session_ids
            .iter()
            .filter(|session_id| {
                tasks.get_task(session_id).is_ok_and(|task| {
                    task.library_item_id == HlsCacheStore::completed_library_item_id(session_id)
                })
            })
            .cloned()
            .collect();
        if hls_cache_scan_succeeded {
            let _failed_hls_session_ids = tasks.fail_unrestorable_playback_tasks(
                &restorable_playback_session_ids,
                &restorable_completed_session_ids,
            );
        }
        if tasks.persistence_available() && hls_cache_scan_succeeded {
            restored_hls_sessions.retain(|session| {
                restored_hls_session_is_authorized(
                    &tasks,
                    &session.id,
                    &restorable_completed_session_ids,
                )
            });
        } else {
            restored_hls_sessions.clear();
        }
        for session in &restored_hls_sessions {
            refresh_restored_hls_playback_source(
                &tasks,
                &playback_uri_factory,
                session,
                &restorable_completed_session_ids,
            );
            if restorable_completed_session_ids.contains(&session.id) {
                hls_sessions.insert(sanitized_completed_session(session));
            } else {
                hls_sessions.insert(session.clone());
            }
        }
        let hls_upstream_client = build_hls_upstream_client();
        let playback_planner = playback_planner_factory(Arc::clone(&options), Arc::clone(&library));
        let playback_planning_permits = Arc::new(Semaphore::new(
            options.bilibili_worker_max_concurrent_tasks.max(1),
        ));
        let hls_cache_finalization_permits =
            Arc::new(Semaphore::new(HLS_CACHE_FINALIZATION_MAX_CONCURRENT_TASKS));
        let hls_fill_scheduler = HlsFillScheduler::default();

        let state = Self {
            options,
            library,
            playback_uri_factory,
            tasks,
            hls_sessions,
            hls_cache,
            hls_upstream_client,
            playback_planner,
            playback_planning_permits,
            hls_cache_finalization_permits,
            hls_fill_scheduler,
            completed_hls_cache_playback_supported,
            last_hls_cache_eviction: Arc::new(Mutex::new(None)),
            hls_cache_quota_enforcement_lock: Arc::new(Mutex::new(())),
            hls_cache_eviction_protected_session_ids: Arc::new(Mutex::new(HashMap::new())),
            hls_cache_playback_leases: Arc::new(Mutex::new(HashMap::new())),
        };
        state.resume_incomplete_hls_cache_finalizers(
            &restored_hls_sessions,
            &completed_cache_session_ids,
        );
        state
    }

    fn resume_incomplete_hls_cache_finalizers(
        &self,
        restored_sessions: &[crate::hls::HlsPlaybackSession],
        completed_session_ids: &HashSet<String>,
    ) {
        if !self.supports_completed_hls_cache_playback() {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        for session in restored_sessions {
            let Ok(task) = self.tasks.get_task(&session.id) else {
                continue;
            };
            if task.state() != TaskState::Playable {
                continue;
            }
            if completed_session_ids.contains(&session.id) {
                self.hls_sessions
                    .insert(sanitized_completed_session(session));
                match self.hls_cache.save_completed_session(session) {
                    Ok(()) => {
                        match self.tasks.complete_playback_cached(
                            &session.id,
                            HlsCacheStore::completed_library_item_id(&session.id),
                        ) {
                            Ok(task) if task.state() == TaskState::Completed => {
                                if let Err(error) = self.enforce_hls_cache_quota(
                                    "after_hls_finalization",
                                    [session.id.clone()],
                                    0,
                                ) {
                                    eprintln!(
                                        "Failed to run HLS cache eviction after startup finalization for task {}: {error}",
                                        session.id
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(status) => {
                                eprintln!(
                                    "Failed to mark restored HLS playback task {} cached during startup restore: {status}",
                                    session.id
                                );
                            }
                        }
                        continue;
                    }
                    Err(error) => {
                        eprintln!(
                            "Failed to sanitize completed HLS cache session {} during startup restore: {error}",
                            session.id
                        );
                    }
                }
            }

            self.enqueue_hls_cache_fill_demoted(
                session.id.clone(),
                session.clone(),
                HlsCacheFinalizationFailureMode::FailRestoredTask,
            );
        }
    }

    pub(crate) fn enqueue_hls_cache_fill_foreground(
        &self,
        task_id: String,
        session: HlsPlaybackSession,
        failure_mode: HlsCacheFinalizationFailureMode,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            eprintln!("HLS cache fill worker could not start outside a Tokio runtime.");
            return;
        };
        let should_start_worker =
            self.hls_fill_scheduler
                .enqueue_foreground(task_id, session, failure_mode);
        if should_start_worker {
            handle.spawn(crate::grpc_services::run_hls_cache_fill_worker(
                self.clone(),
            ));
        }
    }

    pub(crate) fn enqueue_hls_cache_fill_demoted(
        &self,
        task_id: String,
        session: HlsPlaybackSession,
        failure_mode: HlsCacheFinalizationFailureMode,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            eprintln!("HLS cache fill worker could not start outside a Tokio runtime.");
            return;
        };
        let should_start_worker =
            self.hls_fill_scheduler
                .enqueue_demoted(task_id, session, failure_mode);
        if should_start_worker {
            handle.spawn(crate::grpc_services::run_hls_cache_fill_worker(
                self.clone(),
            ));
        }
    }

    pub fn spawn_bilibili_task_worker(
        &self,
        adapter: Arc<dyn BilibiliDownloadAdapter>,
        max_concurrent_tasks: usize,
    ) -> JoinHandle<()> {
        tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&self.tasks),
            adapter,
            max_concurrent_tasks,
        ))
    }

    pub fn spawn_configured_bilibili_task_worker(&self) -> Option<JoinHandle<()>> {
        if !self.options.bilibili_worker_enabled {
            return None;
        }

        Some(self.spawn_bilibili_task_worker(
            Arc::new(BbdownBilibiliAdapter::new(
                Arc::clone(&self.options),
                Arc::clone(&self.library),
            )),
            BBDOWN_WORKER_MAX_CONCURRENT_TASKS,
        ))
    }

    pub(crate) fn list_completed_hls_library_items(&self) -> Vec<LibraryItem> {
        if !self.supports_completed_hls_cache_playback() {
            return Vec::new();
        }
        self.hls_cache
            .list_completed_library_items()
            .into_iter()
            .filter(|item| self.completed_hls_task_is_authorized(&item.source_id))
            .collect()
    }

    pub(crate) fn get_completed_hls_library_item(&self, item_id: &str) -> Option<LibraryItem> {
        if !self.supports_completed_hls_cache_playback() {
            return None;
        }
        let session_id = HlsCacheStore::session_id_from_library_item_id(item_id)?;
        if !self.completed_hls_task_is_authorized(&session_id) {
            return None;
        }
        self.hls_cache.get_completed_library_item(item_id)
    }

    pub(crate) fn create_completed_hls_playback_source(
        &self,
        item_id: &str,
        variant_id: &str,
        uri: String,
    ) -> Option<PlaybackSource> {
        let _quota_lock = self
            .hls_cache_quota_enforcement_lock
            .lock()
            .expect("HLS cache quota enforcement lock poisoned");
        if !self.supports_completed_hls_cache_playback() {
            return None;
        }
        let session_id = HlsCacheStore::session_id_from_library_item_id(item_id)?;
        if !self.completed_hls_task_is_authorized(&session_id) {
            return None;
        }
        let source = self
            .hls_cache
            .create_playback_source(item_id, variant_id, uri)?;
        self.note_hls_cache_playback_use(&session_id);
        Some(source)
    }

    pub(crate) fn hls_playback_session_for_serving(
        &self,
        session_id: &str,
    ) -> Option<HlsPlaybackSession> {
        let _quota_lock = self
            .hls_cache_quota_enforcement_lock
            .lock()
            .expect("HLS cache quota enforcement lock poisoned");
        let session = self.hls_playback_session(session_id)?;
        self.note_hls_cache_playback_use(session_id);
        Some(session)
    }

    pub(crate) fn delete_completed_hls_library_item(
        &self,
        item_id: &str,
    ) -> Result<Option<bool>, Status> {
        let Some(session_id) = HlsCacheStore::session_id_from_library_item_id(item_id) else {
            return Ok(None);
        };
        if !self.supports_completed_hls_cache_playback() {
            return Ok(Some(false));
        }
        if self.get_completed_hls_library_item(item_id).is_none() {
            return Ok(Some(false));
        }

        self.hls_cache
            .remove_session(&session_id)
            .map_err(|error| {
                Status::internal(format!(
                    "Failed to delete completed HLS cache item: {error}"
                ))
            })?;
        self.hls_sessions.remove(&session_id);
        self.tasks
            .remove_completed_playback_task(&session_id, item_id)?;
        Ok(Some(true))
    }

    fn completed_hls_task_is_authorized(&self, session_id: &str) -> bool {
        if !self.supports_completed_hls_cache_playback() {
            return false;
        }
        let Ok(task) = self.tasks.get_task(session_id) else {
            return false;
        };

        if task.kind() != TaskKind::BilibiliProgressivePlayback
            || task.state() != TaskState::Completed
        {
            return false;
        }
        if task.library_item_id != HlsCacheStore::completed_library_item_id(session_id) {
            self.fail_completed_hls_task_after_cache_restore(session_id);
            return false;
        }
        self.ensure_completed_hls_session_registered(session_id)
    }

    pub(crate) fn supports_completed_hls_cache_playback(&self) -> bool {
        self.completed_hls_cache_playback_supported
    }

    pub(crate) fn hls_cache_policy(&self) -> HlsCacheEvictionPolicy {
        HlsCacheEvictionPolicy {
            max_bytes: self.options.hls_cache_max_bytes,
            high_watermark_percent: self.options.hls_cache_high_watermark_percent,
            low_watermark_percent: self.options.hls_cache_low_watermark_percent,
        }
    }

    pub(crate) fn hls_cache_status(&self) -> io::Result<HlsCacheStatusSnapshot> {
        Ok(HlsCacheStatusSnapshot {
            policy: self.hls_cache_policy(),
            usage: self.hls_cache.usage_snapshot()?,
            last_eviction: self
                .last_hls_cache_eviction
                .lock()
                .expect("HLS cache eviction summary lock poisoned")
                .clone(),
        })
    }

    pub(crate) fn protect_hls_cache_session_from_eviction(
        &self,
        session_id: &str,
    ) -> HlsCacheEvictionProtectionGuard {
        let session_id = session_id.to_owned();
        {
            let mut protected_session_ids = self
                .hls_cache_eviction_protected_session_ids
                .lock()
                .expect("HLS cache eviction protection lock poisoned");
            *protected_session_ids.entry(session_id.clone()).or_insert(0) += 1;
        }
        HlsCacheEvictionProtectionGuard {
            session_id,
            protected_session_ids: Arc::clone(&self.hls_cache_eviction_protected_session_ids),
        }
    }

    pub(crate) fn note_hls_cache_playback_use(&self, session_id: &str) {
        let expires_at = SystemTime::now()
            .checked_add(HLS_CACHE_PLAYBACK_LEASE_DURATION)
            .unwrap_or_else(SystemTime::now);
        self.hls_cache_playback_leases
            .lock()
            .expect("HLS cache playback lease lock poisoned")
            .insert(session_id.to_owned(), expires_at);
    }

    fn recently_used_hls_cache_session_ids(&self) -> HashSet<String> {
        let now = SystemTime::now();
        let mut playback_leases = self
            .hls_cache_playback_leases
            .lock()
            .expect("HLS cache playback lease lock poisoned");
        playback_leases.retain(|_, expires_at| *expires_at > now);
        playback_leases.keys().cloned().collect()
    }

    fn hls_cache_session_has_finalization_protection(&self, session_id: &str) -> bool {
        self.hls_cache_eviction_protected_session_ids
            .lock()
            .expect("HLS cache eviction protection lock poisoned")
            .contains_key(session_id)
    }

    fn hls_cache_session_is_currently_protected_from_eviction(
        &self,
        session_id: &str,
        stable_protected_session_ids: &HashSet<String>,
    ) -> bool {
        stable_protected_session_ids.contains(session_id)
            || self.hls_cache_session_has_finalization_protection(session_id)
    }

    pub(crate) fn enforce_hls_cache_quota(
        &self,
        reason: &str,
        protected_session_ids: impl IntoIterator<Item = String>,
        projected_added_bytes: u64,
    ) -> io::Result<Option<HlsCacheEvictionSummary>> {
        self.enforce_hls_cache_quota_until_cancelled(
            reason,
            protected_session_ids,
            projected_added_bytes,
            || false,
        )
    }

    pub(crate) fn enforce_hls_cache_quota_until_cancelled(
        &self,
        reason: &str,
        protected_session_ids: impl IntoIterator<Item = String>,
        projected_added_bytes: u64,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<Option<HlsCacheEvictionSummary>> {
        let policy = self.hls_cache_policy();
        if !policy.eviction_enabled() {
            return Ok(None);
        }
        if should_cancel() {
            return Ok(None);
        }

        let _quota_lock = self
            .hls_cache_quota_enforcement_lock
            .lock()
            .expect("HLS cache quota enforcement lock poisoned");
        if should_cancel() {
            return Ok(None);
        }
        let entries = self.hls_cache.completed_cache_entries()?;
        let partial_entries = self.hls_cache.partial_cache_entries()?;
        let usage = self.hls_cache.usage_snapshot()?;
        let started_used_bytes = usage.used_bytes;
        let projected_used_bytes = started_used_bytes.saturating_add(projected_added_bytes);
        if projected_used_bytes <= policy.high_watermark_bytes() {
            return Ok(None);
        }

        let explicitly_protected_session_ids =
            protected_session_ids.into_iter().collect::<HashSet<_>>();
        let recent_playback_session_ids = self.recently_used_hls_cache_session_ids();
        let mut stable_protected_session_ids = explicitly_protected_session_ids.clone();
        stable_protected_session_ids.extend(self.tasks.protected_hls_cache_session_ids());
        stable_protected_session_ids.extend(recent_playback_session_ids.iter().cloned());
        let mut partial_protected_session_ids = explicitly_protected_session_ids;
        partial_protected_session_ids.extend(recent_playback_session_ids);
        let target_used_bytes = policy
            .low_watermark_bytes()
            .saturating_sub(projected_added_bytes);
        let mut finished_used_bytes = started_used_bytes;
        let mut evicted_bytes = 0_u64;
        let mut evicted_session_ids = Vec::new();

        let mut cancelled = false;
        for entry in entries {
            if finished_used_bytes <= target_used_bytes {
                break;
            }
            if should_cancel() {
                cancelled = true;
                break;
            }
            if self.hls_cache_session_is_currently_protected_from_eviction(
                &entry.session_id,
                &stable_protected_session_ids,
            ) {
                continue;
            }
            if !self.completed_hls_cache_entry_is_evictable(&entry) {
                continue;
            }
            if should_cancel() {
                cancelled = true;
                break;
            }
            if self.hls_cache_session_is_currently_protected_from_eviction(
                &entry.session_id,
                &stable_protected_session_ids,
            ) {
                continue;
            }
            self.hls_cache.remove_session(&entry.session_id)?;
            self.hls_sessions.remove(&entry.session_id);
            self.remove_evicted_completed_hls_task(&entry);
            finished_used_bytes = finished_used_bytes.saturating_sub(entry.size_bytes);
            evicted_bytes = evicted_bytes.saturating_add(entry.size_bytes);
            evicted_session_ids.push(entry.session_id);
        }
        for entry in partial_entries {
            if finished_used_bytes <= target_used_bytes {
                break;
            }
            if should_cancel() {
                cancelled = true;
                break;
            }
            if self.hls_cache_session_is_currently_protected_from_eviction(
                &entry.session_id,
                &partial_protected_session_ids,
            ) {
                continue;
            }
            self.hls_cache
                .remove_session_managed_resources(&entry.session_id)?;
            finished_used_bytes = finished_used_bytes.saturating_sub(entry.size_bytes);
            evicted_bytes = evicted_bytes.saturating_add(entry.size_bytes);
            evicted_session_ids.push(entry.session_id);
        }

        if cancelled && evicted_session_ids.is_empty() {
            return Ok(None);
        }

        Ok(Some(self.record_hls_cache_eviction_summary(
            HlsCacheEvictionSummary {
                reason: reason.to_owned(),
                started_used_bytes,
                finished_used_bytes,
                target_used_bytes,
                projected_added_bytes,
                evicted_bytes,
                evicted_session_ids,
                target_reached: finished_used_bytes.saturating_add(projected_added_bytes)
                    <= policy.low_watermark_bytes(),
                completed_at: SystemTime::now(),
            },
        )))
    }

    fn record_hls_cache_eviction_summary(
        &self,
        summary: HlsCacheEvictionSummary,
    ) -> HlsCacheEvictionSummary {
        *self
            .last_hls_cache_eviction
            .lock()
            .expect("HLS cache eviction summary lock poisoned") = Some(summary.clone());
        summary
    }

    fn completed_hls_cache_entry_is_evictable(&self, entry: &HlsCacheCompletedEntry) -> bool {
        let Ok(task) = self.tasks.get_task(&entry.session_id) else {
            return self.tasks.persistence_available();
        };
        if task.kind() != TaskKind::BilibiliProgressivePlayback {
            return false;
        }
        match task.state() {
            TaskState::Completed => {
                if task.library_item_id == entry.library_item_id {
                    return true;
                }

                self.fail_completed_hls_task_after_cache_restore(&entry.session_id);
                true
            }
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled => true,
            _ => false,
        }
    }

    fn remove_evicted_completed_hls_task(&self, entry: &HlsCacheCompletedEntry) {
        let Ok(task) = self.tasks.get_task(&entry.session_id) else {
            return;
        };
        if task.kind() != TaskKind::BilibiliProgressivePlayback
            || task.state() != TaskState::Completed
            || task.library_item_id != entry.library_item_id
        {
            return;
        }
        if let Err(status) = self
            .tasks
            .remove_completed_playback_task(&entry.session_id, &entry.library_item_id)
        {
            eprintln!(
                "Failed to remove evicted HLS playback task {} after cache eviction: {status}",
                entry.session_id
            );
        }
    }

    pub fn spawn_hls_cache_quota_monitor(&self) -> Option<JoinHandle<()>> {
        if !self.hls_cache_policy().eviction_enabled() {
            return None;
        }

        let state = self.clone();
        Some(tokio::spawn(async move {
            let start = tokio::time::Instant::now() + HLS_CACHE_EVICTION_CHECK_INTERVAL;
            let mut interval = tokio::time::interval_at(start, HLS_CACHE_EVICTION_CHECK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(error) = state.enforce_hls_cache_quota("periodic", Vec::new(), 0) {
                    eprintln!("Failed to run periodic HLS cache eviction: {error}");
                }
            }
        }))
    }

    pub(crate) fn hls_playback_session(&self, session_id: &str) -> Option<HlsPlaybackSession> {
        if let Some(session) = self.hls_sessions.get(session_id) {
            return Some(session);
        }
        self.ensure_hls_session_registered(session_id)
            .then(|| self.hls_sessions.get(session_id))
            .flatten()
    }

    fn ensure_hls_session_registered(&self, session_id: &str) -> bool {
        if self.hls_sessions.get(session_id).is_some() {
            return true;
        }
        let Ok(task) = self.tasks.get_task(session_id) else {
            return false;
        };
        if task.kind() != TaskKind::BilibiliProgressivePlayback {
            return false;
        }

        match task.state() {
            TaskState::Playable => {
                let Some(session) = self.hls_cache.playback_session(session_id) else {
                    return false;
                };
                self.hls_sessions.insert(session);
                true
            }
            TaskState::Completed => {
                if !self.supports_completed_hls_cache_playback() {
                    return false;
                }
                if task.library_item_id != HlsCacheStore::completed_library_item_id(session_id) {
                    self.fail_completed_hls_task_after_cache_restore(session_id);
                    return false;
                }
                self.ensure_completed_hls_session_registered(session_id)
            }
            _ => false,
        }
    }

    fn ensure_completed_hls_session_registered(&self, session_id: &str) -> bool {
        if self.hls_sessions.get(session_id).is_some() {
            return true;
        }
        let Some(session) = self.hls_cache.completed_session(session_id) else {
            return false;
        };
        self.hls_sessions
            .insert(sanitized_completed_session(&session));
        true
    }

    fn fail_completed_hls_task_after_cache_restore(&self, session_id: &str) {
        self.hls_sessions.remove(session_id);
        if let Err(status) = self.tasks.fail_completed_playback_task_after_cache_restore(
            session_id,
            "Restored completed HLS cache item did not match the persisted playback task."
                .to_owned(),
        ) {
            eprintln!(
                "Failed to mark completed HLS playback task {session_id} failed after cache restore validation: {status}"
            );
        }
    }
}

fn refresh_restored_hls_playback_source(
    tasks: &BilibiliTaskRegistry,
    playback_uri_factory: &PlaybackUriFactory,
    session: &crate::hls::HlsPlaybackSession,
    completed_session_ids: &HashSet<String>,
) {
    let Ok(task) = tasks.get_task(&session.id) else {
        return;
    };
    if task.kind() != TaskKind::BilibiliProgressivePlayback {
        return;
    }

    let item_id = match task.state() {
        TaskState::Playable => session.id.clone(),
        TaskState::Completed if completed_session_ids.contains(&session.id) => {
            HlsCacheStore::completed_library_item_id(&session.id)
        }
        _ => return,
    };
    let playback_source = PlaybackSource {
        item_id,
        variant_id: session.variant.id.clone(),
        protocol: PlaybackProtocol::Hls.into(),
        uri: playback_uri_factory.create_hls_master_playlist_for_restored_task(
            &session.id,
            task.playback_source
                .as_ref()
                .map(|source| source.uri.as_str()),
        ),
        expires_at: None,
    };

    if let Err(status) = tasks.refresh_playback_source(&session.id, playback_source) {
        eprintln!(
            "Failed to refresh restored HLS playback source for task {}: {status}",
            session.id
        );
    }
}

fn restored_hls_session_is_authorized(
    tasks: &BilibiliTaskRegistry,
    session_id: &str,
    completed_session_ids: &HashSet<String>,
) -> bool {
    let Ok(task) = tasks.get_task(session_id) else {
        return false;
    };
    if task.kind() != TaskKind::BilibiliProgressivePlayback {
        return false;
    }

    match task.state() {
        TaskState::Playable => true,
        TaskState::Completed => {
            completed_session_ids.contains(session_id)
                && task.library_item_id == HlsCacheStore::completed_library_item_id(session_id)
        }
        _ => false,
    }
}

fn build_hls_upstream_client() -> reqwest::Client {
    // Use a read timeout instead of a whole-request timeout so long segment streams
    // can continue as long as the upstream keeps making progress.
    reqwest::Client::builder()
        .connect_timeout(HLS_UPSTREAM_CONNECT_TIMEOUT)
        .read_timeout(HLS_UPSTREAM_READ_TIMEOUT)
        .pool_idle_timeout(Some(HLS_UPSTREAM_POOL_IDLE_TIMEOUT))
        .build()
        .expect("HLS upstream client should build")
}

pub async fn run(
    options: CacheServerOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = AppState::new(options);
    run_with_state(state).await
}

pub async fn run_with_state(
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let grpc_addrs = state.options.grpc_listen_addrs()?;
    let media_addrs = state.options.media_listen_addrs()?;
    let grpc_listeners = bind_listener_group(grpc_addrs).await?;
    let media_listeners = bind_listener_group(media_addrs).await?;
    let grpc_state = state.clone();
    let media_state = state.clone();
    let mut grpc_servers = spawn_servers(grpc_listeners, grpc_state, run_grpc_listener);
    let mut media_servers = spawn_servers(media_listeners, media_state, run_media_listener);

    let _bonjour_advertisement = match bonjour::BonjourAdvertisement::start(&state.options) {
        Ok(advertisement) => advertisement,
        Err(error) => {
            eprintln!("warning: Bonjour advertisement is unavailable: {error}");
            None
        }
    };
    let _bilibili_worker_task = state.spawn_configured_bilibili_task_worker();
    let _hls_cache_quota_monitor = state.spawn_hls_cache_quota_monitor();

    tokio::select! {
        result = wait_for_server_result(&mut grpc_servers) => result,
        result = wait_for_server_result(&mut media_servers) => result,
        _ = shutdown_signal() => Ok(()),
    }
}

pub async fn run_grpc_servers(
    addrs: Vec<SocketAddr>,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listeners = bind_listener_group(addrs).await?;
    run_servers(listeners, state, run_grpc_listener).await
}

pub async fn run_grpc_server(
    addr: SocketAddr,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = bind_tcp_listener(addr).await?;
    run_grpc_listener(listener, state).await
}

#[doc(hidden)]
pub async fn run_grpc_listener(
    listener: TcpListener,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Server::builder()
        .add_service(ServerServiceServer::new(ServerGrpcService::new(
            state.clone(),
        )))
        .add_service(LibraryServiceServer::new(LibraryGrpcService::new(
            state.clone(),
        )))
        .add_service(TaskServiceServer::new(TaskGrpcService::new(state.clone())))
        .add_service(CacheServiceServer::new(CacheGrpcService::new(state)))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}

pub async fn run_media_servers(
    addrs: Vec<SocketAddr>,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listeners = bind_listener_group(addrs).await?;
    run_servers(listeners, state, run_media_listener).await
}

pub async fn run_media_server(
    addr: SocketAddr,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = bind_tcp_listener(addr).await?;
    run_media_listener(listener, state).await
}

#[doc(hidden)]
pub async fn run_media_listener(
    listener: TcpListener,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let router = Router::new()
        .route("/", get(root))
        .route(
            "/media/{item_id}/{variant_id}",
            get(media_get).head(media_head),
        )
        .route(
            "/hls/{session_id}/master.m3u8",
            get(hls_master_playlist_get).head(hls_master_playlist_head),
        )
        .route(
            "/hls/{session_id}/segments/{segment_id}",
            get(hls_segment_get).head(hls_segment_head),
        )
        .with_state(MediaState::new(state));

    axum::serve(listener, router).await?;
    Ok(())
}

async fn run_servers<F, Fut>(
    listeners: Vec<TcpListener>,
    state: AppState,
    run_one: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(TcpListener, AppState) -> Fut + Copy + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>
        + Send
        + 'static,
{
    let mut servers = spawn_servers(listeners, state, run_one);
    wait_for_server_result(&mut servers).await
}

type ServerTaskResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn spawn_servers<F, Fut>(
    listeners: Vec<TcpListener>,
    state: AppState,
    run_one: F,
) -> JoinSet<ServerTaskResult>
where
    F: Fn(TcpListener, AppState) -> Fut + Copy + Send + Sync + 'static,
    Fut: std::future::Future<Output = ServerTaskResult> + Send + 'static,
{
    let mut servers = JoinSet::new();
    for listener in listeners {
        let state = state.clone();
        servers.spawn(async move { run_one(listener, state).await });
    }

    servers
}

async fn wait_for_server_result(
    servers: &mut JoinSet<ServerTaskResult>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match servers
        .join_next()
        .await
        .expect("at least one listen address is required")
    {
        Ok(result) => result,
        Err(error) => Err(Box::new(error)),
    }
}

async fn root() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "service": "TVOSNetPlayer.CacheServer",
        "controlPlane": "gRPC",
        "mediaPlane": "HTTP"
    }))
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn bind_tcp_listener(addr: SocketAddr) -> io::Result<TcpListener> {
    const TCP_BACKLOG: i32 = 1024;

    let socket = match addr {
        SocketAddr::V4(_) => Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?,
        SocketAddr::V6(_) => {
            let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
            socket.set_only_v6(true)?;
            socket
        }
    };

    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket
        .listen(TCP_BACKLOG)
        .and_then(|()| TcpListener::from_std(socket.into()))
}

async fn bind_listener_group(addrs: Vec<SocketAddr>) -> io::Result<Vec<TcpListener>> {
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "at least one listen address is required",
        ));
    }

    let allow_optional_family_skip = addrs.len() > 1;
    let mut listeners = Vec::with_capacity(addrs.len());
    let mut first_optional_error = None;

    for addr in addrs {
        match bind_tcp_listener(addr).await {
            Ok(listener) => listeners.push(listener),
            Err(error)
                if allow_optional_family_skip && is_optional_address_family_unavailable(&error) =>
            {
                if first_optional_error.is_none() {
                    first_optional_error = Some(error);
                }
            }
            Err(error) => return Err(error),
        }
    }

    if listeners.is_empty() {
        return Err(first_optional_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no listen address is usable",
            )
        }));
    }

    Ok(listeners)
}

fn is_optional_address_family_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
    )
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
    };

    use super::*;

    #[tokio::test]
    async fn binds_ipv4_and_ipv6_wildcard_on_same_port() {
        let port = match free_port() {
            Ok(port) => port,
            Err(error) if is_listener_unavailable(&error) => return,
            Err(error) => panic!("free port probe should bind: {error}"),
        };
        let ipv4_listener =
            bind_tcp_listener(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
                .await
                .expect("IPv4 wildcard listener should bind");

        match bind_tcp_listener(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)).await {
            Ok(ipv6_listener) => {
                assert_eq!(port, ipv6_listener.local_addr().unwrap().port());
                drop(ipv6_listener);
            }
            Err(error) if is_listener_unavailable(&error) => {}
            Err(error) => panic!("IPv6 wildcard listener should bind with v6-only mode: {error}"),
        }

        drop(ipv4_listener);
    }

    #[tokio::test]
    async fn listener_group_keeps_available_family_when_other_family_is_unavailable() {
        let port = match free_port() {
            Ok(port) => port,
            Err(error) if is_listener_unavailable(&error) => return,
            Err(error) => panic!("free port probe should bind: {error}"),
        };
        let listeners = bind_listener_group(vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            SocketAddr::new("2001:db8::1".parse().unwrap(), port),
        ])
        .await
        .expect("listener group should keep the available address family");

        assert_eq!(1, listeners.len());
        assert_eq!(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            listeners[0].local_addr().unwrap()
        );
    }

    fn free_port() -> io::Result<u16> {
        Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
    }

    fn is_listener_unavailable(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::PermissionDenied
                | io::ErrorKind::Unsupported
        )
    }
}
