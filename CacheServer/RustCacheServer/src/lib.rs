mod bbdown_adapter;
mod bilibili_playback;
pub mod bilibili_worker;
mod bonjour;
mod codecs;
pub mod config;
pub mod generated;
pub mod grpc_services;
mod hls;
mod hls_cache;
mod hls_fill_scheduler;
mod hls_network_policy;
mod hls_playback_progress;
pub mod library;
pub mod media;
mod mp4_segments;
pub mod playback;
mod playback_policy;
mod task_output;
pub mod task_registry;
mod task_store;
mod transcoding;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant as MonotonicInstant, SystemTime},
};

use axum::{Router, routing::get};
use bbdown_adapter::BbdownBilibiliAdapter;
use bilibili_worker::{BilibiliDownloadAdapter, run_bilibili_task_worker};
use generated::tvos_net_player::v1::{
    BilibiliLoginSession, LibraryItem, PlaybackProtocol, PlaybackSource, Task, TaskKind, TaskState,
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
        TaskGrpcService, TaskResultPageStore, playback_session_from_hls_cache_session,
    },
    hls::{HlsPlaybackRegistry, HlsPlaybackSession, HlsPlaybackSessionHandle},
    hls_cache::{
        HlsCacheCompletedEntry, HlsCacheEvictionPolicy, HlsCacheEvictionSummary,
        HlsCacheStatusSnapshot, HlsCacheStore, HlsTranscodingExecutionConfig,
        completed_runtime_session, sanitized_completed_session,
        source_completed_session_for_restore,
    },
    hls_fill_scheduler::HlsFillScheduler,
    hls_network_policy::{HlsNetworkPolicy, HlsWeakNetworkSnapshot},
    hls_playback_progress::{
        HlsPlaybackProgressSnapshot, HlsPlaybackProgressTracker, PlaybackProgressIntent,
        PlaybackProgressRecordOutcome, PlaybackProgressReport, session_id_from_report,
    },
    library::LocalMediaLibrary,
    media::{
        MediaState, hls_master_playlist_get, hls_master_playlist_head, hls_segment_get,
        hls_segment_head, media_get, media_head, resource_get, resource_head,
    },
    playback::PlaybackUriFactory,
    task_registry::BilibiliTaskRegistry,
    transcoding::HlsTranscodingPlanState,
};

const BBDOWN_WORKER_MAX_CONCURRENT_TASKS: usize = 1;
const HLS_CACHE_FINALIZATION_MAX_CONCURRENT_TASKS: usize = 1;
const TASK_RESOURCE_OPEN_MAX_CONCURRENT_JOBS: usize = 32;
const HLS_UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HLS_UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(20);
const HLS_UPSTREAM_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const HLS_CACHE_EVICTION_CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);
const HLS_CACHE_PLAYBACK_LEASE_DURATION: Duration = Duration::from_secs(15 * 60);
const HLS_COMPLETION_STALE_CLIENT_GRACE_PERIOD: Duration = Duration::from_secs(60);
const CREDENTIAL_SAFE_LOG_DETAIL: &str =
    "detail omitted because Bilibili credential material is configured";
pub(crate) const CREDENTIAL_SAFE_CLIENT_DETAIL: &str =
    "Bilibili error detail omitted because credential material is configured.";
pub(crate) const CREDENTIAL_SAFE_CLIENT_RUNNING_DETAIL: &str =
    "Bilibili download progress detail omitted because credential material is configured.";
pub(crate) const CREDENTIAL_SAFE_CLIENT_CANCELLATION_DETAIL: &str =
    "Bilibili cancellation detail omitted because credential material is configured.";
const BILIBILI_FAILURE_CLASS_TAG: &str = "bilibili_failure_class";

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
    playback_planning_active_jobs: Arc<AtomicUsize>,
    pub(crate) hls_cache_finalization_permits: Arc<Semaphore>,
    pub(crate) task_resource_open_permits: Arc<Semaphore>,
    pub(crate) lan_transcoding_permits: Arc<Semaphore>,
    pub(crate) lan_transcoding_active_jobs: Arc<AtomicUsize>,
    pub(crate) hls_fill_scheduler: HlsFillScheduler,
    pub(crate) hls_network_policy: HlsNetworkPolicy,
    pub(crate) hls_playback_progress: HlsPlaybackProgressTracker,
    pub(crate) bilibili_login_sessions: Arc<Mutex<VecDeque<BilibiliLoginSession>>>,
    pub(crate) task_result_pages: Arc<Mutex<TaskResultPageStore>>,
    pub(crate) completed_hls_cache_playback_supported: bool,
    pub(crate) last_hls_cache_eviction: Arc<Mutex<Option<HlsCacheEvictionSummary>>>,
    hls_cache_quota_enforcement_lock: Arc<Mutex<()>>,
    hls_cache_eviction_protected_session_ids: Arc<Mutex<HashMap<String, usize>>>,
    hls_cache_playback_leases: Arc<Mutex<HashMap<String, SystemTime>>>,
    completed_hls_deletion_lock: Arc<Mutex<()>>,
    pending_hls_session_cleanup_by_library_item: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

pub(crate) struct HlsCacheEvictionProtectionGuard {
    session_id: String,
    protected_session_ids: Arc<Mutex<HashMap<String, usize>>>,
}

pub(crate) struct PlaybackPlanningActivityGuard {
    active_jobs: Arc<AtomicUsize>,
}

impl Drop for PlaybackPlanningActivityGuard {
    fn drop(&mut self) {
        self.active_jobs.fetch_sub(1, Ordering::SeqCst);
    }
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
        let tasks = Arc::new(
            BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
                task_state_path,
                task_retention_policy,
                Some(options.root_path.clone()),
            ),
        );
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
        let source_completed_restore_session_ids = if completed_hls_cache_playback_supported {
            restored_hls_sessions
                .iter()
                .filter(|session| {
                    let library_item_id = HlsCacheStore::completed_library_item_id(&session.id);
                    session.transcoding.state == HlsTranscodingPlanState::Ready
                        && hls_cache.source_resources_are_complete(session)
                        && tasks
                            .playback_task_for_any_hls_session(&session.id)
                            .is_some_and(|task| {
                                tasks.playback_task_has_completed_hls_cache_item(
                                    &task,
                                    &session.id,
                                    &library_item_id,
                                )
                            })
                })
                .map(|session| session.id.clone())
                .collect::<HashSet<_>>()
        } else {
            HashSet::new()
        };
        if !source_completed_restore_session_ids.is_empty() {
            for session in &mut restored_hls_sessions {
                if !source_completed_restore_session_ids.contains(&session.id) {
                    continue;
                }
                *session = source_completed_session_for_restore(session);
                if let Err(error) = hls_cache.save_completed_session(session) {
                    eprintln!(
                        "Failed to migrate restored completed HLS source session {}: {error}",
                        session.id
                    );
                }
            }
        }
        let completed_cache_session_ids = if completed_hls_cache_playback_supported {
            hls_cache.completed_session_ids(&restored_hls_sessions)
        } else {
            HashSet::new()
        };
        let restorable_completed_session_ids = completed_cache_session_ids
            .iter()
            .filter(|session_id| {
                let library_item_id = HlsCacheStore::completed_library_item_id(session_id);
                tasks
                    .playback_task_for_any_hls_session(session_id)
                    .is_some_and(|task| {
                        tasks.playback_task_has_completed_hls_cache_item(
                            &task,
                            session_id,
                            &library_item_id,
                        )
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
                let authorized = restored_hls_session_is_authorized(
                    &tasks,
                    &session.id,
                    &restorable_completed_session_ids,
                    completed_hls_cache_playback_supported,
                );
                if !authorized && let Err(error) = hls_cache.remove_session(&session.id) {
                    eprintln!(
                        "Failed to remove unauthorized restored HLS session {}: {error}",
                        session.id
                    );
                }
                authorized
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
        let playback_planning_active_jobs = Arc::new(AtomicUsize::new(0));
        let hls_cache_finalization_permits =
            Arc::new(Semaphore::new(HLS_CACHE_FINALIZATION_MAX_CONCURRENT_TASKS));
        let task_resource_open_permits =
            Arc::new(Semaphore::new(TASK_RESOURCE_OPEN_MAX_CONCURRENT_JOBS));
        let lan_transcoding_permits = Arc::new(Semaphore::new(
            options.lan_transcoding_max_concurrent_jobs.max(1),
        ));
        let lan_transcoding_active_jobs = Arc::new(AtomicUsize::new(0));
        let hls_fill_scheduler = HlsFillScheduler::default();
        let hls_network_policy = HlsNetworkPolicy::default();
        let hls_playback_progress = HlsPlaybackProgressTracker::default();

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
            playback_planning_active_jobs,
            hls_cache_finalization_permits,
            task_resource_open_permits,
            lan_transcoding_permits,
            lan_transcoding_active_jobs,
            hls_fill_scheduler,
            hls_network_policy,
            hls_playback_progress,
            bilibili_login_sessions: Arc::new(Mutex::new(VecDeque::new())),
            task_result_pages: Arc::new(Mutex::new(TaskResultPageStore::default())),
            completed_hls_cache_playback_supported,
            last_hls_cache_eviction: Arc::new(Mutex::new(None)),
            hls_cache_quota_enforcement_lock: Arc::new(Mutex::new(())),
            hls_cache_eviction_protected_session_ids: Arc::new(Mutex::new(HashMap::new())),
            hls_cache_playback_leases: Arc::new(Mutex::new(HashMap::new())),
            completed_hls_deletion_lock: Arc::new(Mutex::new(())),
            pending_hls_session_cleanup_by_library_item: Arc::new(Mutex::new(HashMap::new())),
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
            let Some(task_id) = self.tasks.playable_task_id_for_hls_session(&session.id) else {
                continue;
            };
            if session.transcoding.state == HlsTranscodingPlanState::Ready
                && self.hls_transcoding_execution_config().is_none()
            {
                continue;
            }
            if self
                .tasks
                .hls_session_has_online_playback_after_cache_fill_failure(&task_id, &session.id)
            {
                continue;
            }
            if completed_session_ids.contains(&session.id) {
                self.hls_sessions
                    .insert(sanitized_completed_session(session));
                match self.hls_cache.save_completed_session(session) {
                    Ok(()) => {
                        let completed_playback_session =
                            playback_session_from_hls_cache_session(session);
                        let library_item_id = HlsCacheStore::completed_library_item_id(&session.id);
                        let completion = {
                            let _deletion_guard = self.completed_hls_mutation_guard();
                            self.tasks
                                .complete_playback_hls_session_cached_with_metadata(
                                    &task_id,
                                    &session.id,
                                    library_item_id.clone(),
                                    completed_playback_session,
                                )
                        };
                        match completion {
                            Ok(task)
                                if self.tasks.playback_task_has_completed_hls_cache_item(
                                    &task,
                                    &session.id,
                                    &library_item_id,
                                ) =>
                            {
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
                task_id,
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
            self.options.bbdown_credential_path.is_some(),
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
    ) -> Option<HlsPlaybackSessionHandle> {
        let _quota_lock = self
            .hls_cache_quota_enforcement_lock
            .lock()
            .expect("HLS cache quota enforcement lock poisoned");
        let was_registered = self.hls_sessions.get(session_id).is_some();
        if was_registered && !self.registered_hls_session_is_authorized_for_serving(session_id) {
            self.remove_hls_playback_session(session_id);
            return None;
        }
        if self.hls_playback_session(session_id).is_none() {
            self.fail_unrestorable_hls_playback_session_if_cache_is_accessible(session_id);
            return None;
        }
        let handle = self.hls_sessions.get_with_generation(session_id)?;
        if !was_registered || self.restored_hls_playback_source_needs_refresh(&handle.session) {
            refresh_restored_hls_playback_source_for_session(
                &self.tasks,
                &self.playback_uri_factory,
                &handle.session,
                self.completed_hls_task_is_authorized(session_id),
            );
        }
        self.note_hls_cache_playback_use(session_id);
        Some(handle)
    }

    fn registered_hls_session_is_authorized_for_serving(&self, session_id: &str) -> bool {
        if self.completed_hls_task_is_authorized(session_id) {
            return true;
        }

        let Ok(task) = self.tasks.get_task(session_id) else {
            return self.tasks.is_playback_result_session_playable(
                session_id,
                self.supports_completed_hls_cache_playback(),
            );
        };
        if task.kind() != TaskKind::BilibiliProgressivePlayback {
            return false;
        }

        match task.state() {
            TaskState::Playable => self
                .tasks
                .is_hls_session_playable_for_task(&task.id, session_id),
            TaskState::Completed => false,
            _ => false,
        }
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
        let _quota_guard = self
            .hls_cache_quota_enforcement_lock
            .lock()
            .expect("HLS cache quota enforcement lock poisoned");
        let _deletion_guard = self
            .completed_hls_deletion_lock
            .lock()
            .expect("completed HLS deletion lock poisoned");
        self.ensure_task_state_durable_for_hls_deletion()?;
        let pending_session_ids = self
            .pending_hls_session_cleanup_by_library_item
            .lock()
            .expect("pending HLS cleanup lock poisoned")
            .get(item_id)
            .cloned();
        if let Some(pending_session_ids) = pending_session_ids {
            self.remove_hls_sessions_for_library_item(item_id, &pending_session_ids)?;
            return Ok(Some(true));
        }
        let authorized = self.get_completed_hls_library_item(item_id).is_some();
        if !authorized && self.hls_cache.get_completed_library_item(item_id).is_none() {
            return Ok(Some(false));
        }
        if !authorized
            && self
                .tasks
                .playback_task_for_any_hls_session(&session_id)
                .is_some()
        {
            return Ok(Some(false));
        }
        let session_ids = if authorized {
            let session_ids = self.completed_hls_delete_session_ids(&session_id, item_id);
            let (task_cleanup_session_id, task_cleanup_library_item_id) = self
                .completed_hls_task_cleanup_item(&session_id, item_id)
                .unwrap_or_else(|| (session_id.clone(), item_id.to_owned()));
            if !self.tasks.remove_completed_playback_task(
                &task_cleanup_session_id,
                &task_cleanup_library_item_id,
            )? {
                return Ok(Some(false));
            }
            session_ids
        } else {
            vec![session_id]
        };

        self.remove_hls_sessions_for_library_item(item_id, &session_ids)?;
        Ok(Some(true))
    }

    fn ensure_task_state_durable_for_hls_deletion(&self) -> Result<(), Status> {
        if !self.tasks.persistence_configured()
            || self.tasks.persistence_available()
            || self.tasks.retry_pending_persistence()
        {
            return Ok(());
        }
        Err(Status::unavailable(
            "Task state is not durable enough to delete HLS cache data.",
        ))
    }

    pub(crate) fn completed_hls_mutation_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.completed_hls_deletion_lock
            .lock()
            .expect("completed HLS deletion lock poisoned")
    }

    fn completed_hls_delete_session_ids(
        &self,
        session_id: &str,
        library_item_id: &str,
    ) -> Vec<String> {
        self.tasks
            .completed_playback_task_for_any_hls_session(session_id)
            .filter(|task| {
                task.kind() == TaskKind::BilibiliProgressivePlayback
                    && task.state() == TaskState::Completed
                    && self.completed_hls_cache_entry_belongs_to_task(task, session_id)
                    && !self.completed_hls_cache_entry_is_completed_secondary_result_item(
                        task,
                        session_id,
                        library_item_id,
                    )
            })
            .map(|task| self.tasks.playback_hls_session_ids(&task.id))
            .filter(|session_ids| !session_ids.is_empty())
            .unwrap_or_else(|| vec![session_id.to_owned()])
    }

    fn completed_hls_task_cleanup_item(
        &self,
        session_id: &str,
        library_item_id: &str,
    ) -> Option<(String, String)> {
        let task = self.tasks.playback_task_for_any_hls_session(session_id)?;
        if task.kind() != TaskKind::BilibiliProgressivePlayback
            || !matches!(task.state(), TaskState::Playable | TaskState::Completed)
            || !self.completed_hls_cache_entry_belongs_to_task(&task, session_id)
        {
            return None;
        }
        if self.completed_hls_cache_entry_is_completed_secondary_result_item(
            &task,
            session_id,
            library_item_id,
        ) {
            return Some((session_id.to_owned(), library_item_id.to_owned()));
        }
        if task.state() != TaskState::Completed {
            return None;
        }
        if task.library_item_id.is_empty() {
            return None;
        }
        let removal_session_id =
            HlsCacheStore::session_id_from_library_item_id(&task.library_item_id)
                .or_else(|| {
                    task.playback_session
                        .as_ref()
                        .map(|session| session.id.clone())
                })
                .unwrap_or_else(|| task.id.clone());
        Some((removal_session_id, task.library_item_id))
    }

    #[cfg(test)]
    pub(crate) fn completed_hls_task_cleanup_item_for_tests(
        &self,
        session_id: &str,
        library_item_id: &str,
    ) -> Option<(String, String)> {
        self.completed_hls_task_cleanup_item(session_id, library_item_id)
    }

    fn remove_hls_sessions_for_library_item(
        &self,
        library_item_id: &str,
        session_ids: &[String],
    ) -> Result<(), Status> {
        self.remove_hls_sessions_tracking_failures(library_item_id, session_ids)
            .map_err(|error| {
                Status::internal(format!(
                    "Failed to delete completed HLS cache item: {error}"
                ))
            })
    }

    fn remove_hls_sessions_tracking_failures(
        &self,
        cleanup_key: &str,
        session_ids: &[String],
    ) -> io::Result<()> {
        let (failed_session_ids, first_error) =
            self.remove_hls_sessions_collecting_failures(session_ids);
        let mut pending = self
            .pending_hls_session_cleanup_by_library_item
            .lock()
            .expect("pending HLS cleanup lock poisoned");
        if failed_session_ids.is_empty() {
            pending.remove(cleanup_key);
        } else {
            pending.insert(cleanup_key.to_owned(), failed_session_ids);
        }
        drop(pending);
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    fn retry_pending_hls_session_cleanups(&self) -> HashSet<String> {
        let pending = self
            .pending_hls_session_cleanup_by_library_item
            .lock()
            .expect("pending HLS cleanup lock poisoned")
            .clone();
        for (cleanup_key, session_ids) in pending {
            if let Err(error) =
                self.remove_hls_sessions_tracking_failures(&cleanup_key, &session_ids)
            {
                eprintln!("Failed to retry pending HLS cache cleanup for {cleanup_key}: {error}");
            }
        }
        self.pending_hls_session_cleanup_by_library_item
            .lock()
            .expect("pending HLS cleanup lock poisoned")
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    fn remove_hls_sessions_collecting_failures(
        &self,
        session_ids: &[String],
    ) -> (Vec<String>, Option<io::Error>) {
        let mut removed = HashSet::new();
        let mut failed_session_ids = Vec::new();
        let mut first_error = None;
        for session_id in session_ids {
            if !removed.insert(session_id) {
                continue;
            }
            match self.hls_cache.remove_session(session_id) {
                Ok(()) => self.remove_hls_playback_session(session_id),
                Err(error) => {
                    failed_session_ids.push(session_id.clone());
                    first_error.get_or_insert(error);
                }
            }
        }
        (failed_session_ids, first_error)
    }

    pub(crate) fn register_hls_playback_session(&self, session: HlsPlaybackSession) -> u64 {
        let session_id = session.id.clone();
        self.hls_sessions
            .insert_with_generation_update(session, |generation| {
                self.hls_network_policy
                    .advance_session_generation(&session_id, generation);
            })
    }

    pub(crate) fn remove_hls_playback_session(&self, session_id: &str) {
        self.hls_sessions
            .remove_with_generation_update(session_id, |generation| {
                self.hls_network_policy
                    .remove_session_generation(session_id, generation);
            });
        self.hls_playback_progress.remove_session(session_id);
    }

    pub(crate) fn register_completed_hls_runtime_session(&self, session: &HlsPlaybackSession) {
        self.register_completed_hls_runtime_session_with_grace(
            session,
            HLS_COMPLETION_STALE_CLIENT_GRACE_PERIOD,
        );
    }

    fn register_completed_hls_runtime_session_with_grace(
        &self,
        session: &HlsPlaybackSession,
        grace_period: Duration,
    ) {
        let runtime_session = completed_runtime_session(session);
        let sanitized_session = sanitized_completed_session(session);
        let session_id = runtime_session.id.clone();
        let deadline = MonotonicInstant::now()
            .checked_add(grace_period)
            .expect("HLS completion grace deadline should fit in Instant");
        let generation = self
            .hls_sessions
            .insert_with_scrub_deadline_and_generation_update(
                runtime_session,
                sanitized_session,
                deadline,
                |generation| {
                    self.hls_network_policy
                        .advance_session_generation(&session.id, generation);
                },
            );

        let registry = self.hls_sessions.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace_period).await;
            registry.scrub_generation(&session_id, generation);
        });
    }

    pub(crate) fn begin_playback_planning(&self) -> PlaybackPlanningActivityGuard {
        self.playback_planning_active_jobs
            .fetch_add(1, Ordering::SeqCst);
        PlaybackPlanningActivityGuard {
            active_jobs: Arc::clone(&self.playback_planning_active_jobs),
        }
    }

    pub(crate) fn error_detail_for_log(&self, detail: &dyn Display) -> String {
        error_detail_for_log(self.options.bbdown_credential_path.is_some(), detail)
    }

    pub(crate) fn error_detail_for_client(&self, detail: &dyn Display) -> String {
        credential_safe_client_error(self.options.bbdown_credential_path.is_some(), detail)
    }

    pub(crate) fn cancellation_detail_for_client(&self, detail: &str) -> String {
        credential_safe_client_cancellation(self.options.bbdown_credential_path.is_some(), detail)
    }

    pub(crate) fn error_with_context_for_client(
        &self,
        context: &'static str,
        detail: &dyn Display,
    ) -> String {
        credential_safe_client_error_with_context(
            self.options.bbdown_credential_path.is_some(),
            context,
            detail,
        )
    }

    pub(crate) fn bilibili_error_details_are_sensitive(&self) -> bool {
        self.options.bbdown_credential_path.is_some()
    }

    #[doc(hidden)]
    pub fn background_work_is_idle(&self) -> bool {
        self.playback_planning_active_jobs.load(Ordering::SeqCst) == 0
            && self.playback_planning_permits.available_permits()
                == self.options.bilibili_worker_max_concurrent_tasks.max(1)
            && self.hls_cache_finalization_permits.available_permits()
                == HLS_CACHE_FINALIZATION_MAX_CONCURRENT_TASKS
            && self.lan_transcoding_permits.available_permits()
                == self.options.lan_transcoding_max_concurrent_jobs.max(1)
            && self.lan_transcoding_active_job_count() == 0
            && self.hls_fill_scheduler.is_idle()
    }

    #[doc(hidden)]
    pub fn cancel_hls_fill_work_for_task(&self, task_id: &str) {
        self.hls_fill_scheduler.cancel_task(task_id);
    }

    #[doc(hidden)]
    pub async fn shutdown_hls_fill_worker(&self) {
        self.hls_fill_scheduler.shutdown_and_wait_for_worker().await;
    }

    #[doc(hidden)]
    pub fn background_work_diagnostics(&self) -> String {
        let (hls_fill_current, hls_fill_foreground, hls_fill_demoted) =
            self.hls_fill_scheduler.diagnostic_counts();
        format!(
            "planning_active={}, planning_permits={}/{}, finalization_permits={}/{}, transcoding_active={}, transcoding_permits={}/{}, hls_fill_current={}, hls_fill_foreground={}, hls_fill_demoted={}",
            self.playback_planning_active_jobs.load(Ordering::SeqCst),
            self.playback_planning_permits.available_permits(),
            self.options.bilibili_worker_max_concurrent_tasks.max(1),
            self.hls_cache_finalization_permits.available_permits(),
            HLS_CACHE_FINALIZATION_MAX_CONCURRENT_TASKS,
            self.lan_transcoding_active_job_count(),
            self.lan_transcoding_permits.available_permits(),
            self.options.lan_transcoding_max_concurrent_jobs.max(1),
            hls_fill_current,
            hls_fill_foreground,
            hls_fill_demoted,
        )
    }

    fn completed_hls_task_is_authorized(&self, session_id: &str) -> bool {
        if !self.supports_completed_hls_cache_playback() {
            return false;
        }
        let Some(task) = self.tasks.playback_task_for_any_hls_session(session_id) else {
            return false;
        };

        if task.kind() != TaskKind::BilibiliProgressivePlayback {
            return false;
        }
        let library_item_id = HlsCacheStore::completed_library_item_id(session_id);
        if !self.tasks.playback_task_has_completed_hls_cache_item(
            &task,
            session_id,
            &library_item_id,
        ) {
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

    pub(crate) fn lan_transcoding_active_job_count(&self) -> usize {
        self.lan_transcoding_active_jobs.load(Ordering::SeqCst)
    }

    pub(crate) fn hls_transcoding_execution_config(&self) -> Option<HlsTranscodingExecutionConfig> {
        if !self.options.lan_transcoding_enabled {
            return None;
        }

        Some(HlsTranscodingExecutionConfig {
            ffmpeg_path: self.options.lan_transcoding_ffmpeg_path.clone(),
            permits: Arc::clone(&self.lan_transcoding_permits),
            active_job_count: Arc::clone(&self.lan_transcoding_active_jobs),
        })
    }

    pub(crate) fn hls_weak_network_status(&self) -> HlsWeakNetworkSnapshot {
        self.hls_network_policy.snapshot()
    }

    pub(crate) fn record_hls_playback_progress(
        &self,
        report: PlaybackProgressReport,
    ) -> PlaybackProgressRecordOutcome {
        let Some(session_id) = session_id_from_report(&report) else {
            return PlaybackProgressRecordOutcome {
                accepted: false,
                session_id: String::new(),
                message: "Playback URI does not identify an HLS cache session.".to_owned(),
            };
        };

        let intent = report.intent;
        let should_promote = intent.promotes_hls_cache_fill();
        let restart_current = matches!(intent, PlaybackProgressIntent::Seek);
        if !self.registered_hls_session_is_authorized_for_serving(&session_id) {
            return PlaybackProgressRecordOutcome {
                accepted: false,
                session_id,
                message: "Playback URI does not identify a known HLS cache session.".to_owned(),
            };
        }

        let outcome = self.hls_playback_progress.record(report);
        let promoted_before_quota_lock = outcome.accepted
            && should_promote
            && self.promote_hls_cache_fill_for_playback(&session_id, restart_current);

        {
            let _quota_lock = self
                .hls_cache_quota_enforcement_lock
                .lock()
                .expect("HLS cache quota enforcement lock poisoned");
            if !self.registered_hls_session_is_authorized_for_serving(&session_id) {
                self.hls_playback_progress.remove_session(&session_id);
                return PlaybackProgressRecordOutcome {
                    accepted: false,
                    session_id,
                    message: "Playback URI does not identify a known HLS cache session.".to_owned(),
                };
            }

            if outcome.accepted {
                self.note_hls_cache_playback_use(&outcome.session_id);
            }
        }
        if outcome.accepted && should_promote && !promoted_before_quota_lock {
            self.promote_hls_cache_fill_for_playback(&outcome.session_id, restart_current);
        }
        outcome
    }

    pub(crate) fn hls_playback_progress_status(&self) -> HlsPlaybackProgressSnapshot {
        self.hls_playback_progress.snapshot()
    }

    pub(crate) fn hls_playback_progress_for_session(
        &self,
        session_id: &str,
    ) -> Option<HlsPlaybackProgressSnapshot> {
        self.hls_playback_progress.snapshot_for_session(session_id)
    }

    fn promote_hls_cache_fill_for_playback(&self, session_id: &str, restart_current: bool) -> bool {
        let promoted = self
            .hls_fill_scheduler
            .promote_session_to_foreground(session_id, restart_current);
        if promoted {
            eprintln!("Promoted HLS cache fill for active playback session {session_id}.");
        }
        promoted
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
        let pending_cleanup_session_ids = {
            let _deletion_guard = self.completed_hls_mutation_guard();
            if should_cancel() {
                return Ok(None);
            }
            self.retry_pending_hls_session_cleanups()
        };
        if should_cancel() {
            return Ok(None);
        }
        if !policy.eviction_enabled() {
            return Ok(None);
        }
        let entries = self.hls_cache.completed_cache_entries()?;
        let partial_entries = self.hls_cache.partial_cache_entries()?;
        let completed_entry_sizes_by_session_id = entries
            .iter()
            .map(|entry| (entry.session_id.clone(), entry.size_bytes))
            .collect::<HashMap<_, _>>();
        let partial_entry_sizes_by_session_id = partial_entries
            .iter()
            .map(|entry| (entry.session_id.clone(), entry.size_bytes))
            .collect::<HashMap<_, _>>();
        let usage = self.hls_cache.usage_snapshot()?;
        let started_used_bytes = usage.used_bytes;
        let projected_used_bytes = started_used_bytes.saturating_add(projected_added_bytes);
        if projected_used_bytes <= policy.high_watermark_bytes() {
            return Ok(None);
        }

        let explicitly_protected_session_ids =
            protected_session_ids.into_iter().collect::<HashSet<_>>();
        let recent_playback_session_ids = self.recently_used_hls_cache_session_ids();
        let mut completed_group_protected_session_ids = explicitly_protected_session_ids;
        completed_group_protected_session_ids.extend(recent_playback_session_ids.iter().cloned());
        completed_group_protected_session_ids.extend(pending_cleanup_session_ids);
        let mut stable_protected_session_ids = completed_group_protected_session_ids.clone();
        stable_protected_session_ids.extend(self.tasks.protected_hls_cache_session_ids());
        let partial_protected_session_ids = completed_group_protected_session_ids.clone();
        let target_used_bytes = policy
            .low_watermark_bytes()
            .saturating_sub(projected_added_bytes);
        let mut finished_used_bytes = started_used_bytes;
        let mut evicted_bytes = 0_u64;
        let mut evicted_session_ids = Vec::new();
        let mut evicted_session_id_set = HashSet::new();

        let mut cancelled = false;
        for entry in entries {
            if finished_used_bytes <= target_used_bytes {
                break;
            }
            if evicted_session_id_set.contains(&entry.session_id) {
                continue;
            }
            if should_cancel() {
                cancelled = true;
                break;
            }
            let session_ids =
                self.completed_hls_delete_session_ids(&entry.session_id, &entry.library_item_id);
            if session_ids
                .iter()
                .any(|session_id| evicted_session_id_set.contains(session_id))
            {
                continue;
            }
            let protected_session_ids_for_completed_entry =
                if self.completed_hls_cache_entry_belongs_to_completed_task(&entry) {
                    &completed_group_protected_session_ids
                } else {
                    &stable_protected_session_ids
                };
            if session_ids.iter().any(|session_id| {
                self.hls_cache_session_is_currently_protected_from_eviction(
                    session_id,
                    protected_session_ids_for_completed_entry,
                )
            }) {
                continue;
            }
            if !self.completed_hls_cache_entry_is_evictable(&entry) {
                continue;
            }
            if should_cancel() {
                cancelled = true;
                break;
            }
            if session_ids.iter().any(|session_id| {
                self.hls_cache_session_is_currently_protected_from_eviction(
                    session_id,
                    protected_session_ids_for_completed_entry,
                )
            }) {
                continue;
            }
            let _deletion_guard = self.completed_hls_mutation_guard();
            if should_cancel() {
                cancelled = true;
                break;
            }
            if session_ids.iter().any(|session_id| {
                self.hls_cache_session_is_currently_protected_from_eviction(
                    session_id,
                    protected_session_ids_for_completed_entry,
                )
            }) || !self.completed_hls_cache_entry_is_evictable(&entry)
            {
                continue;
            }
            if !self.remove_evicted_completed_hls_task(&entry)? {
                continue;
            }
            self.remove_hls_sessions_tracking_failures(&entry.library_item_id, &session_ids)?;
            let removed_bytes = session_ids.iter().fold(0_u64, |total, session_id| {
                total.saturating_add(
                    completed_entry_sizes_by_session_id
                        .get(session_id)
                        .or_else(|| partial_entry_sizes_by_session_id.get(session_id))
                        .copied()
                        .unwrap_or_default(),
                )
            });
            finished_used_bytes = finished_used_bytes.saturating_sub(removed_bytes);
            evicted_bytes = evicted_bytes.saturating_add(removed_bytes);
            for session_id in session_ids {
                if evicted_session_id_set.insert(session_id.clone()) {
                    evicted_session_ids.push(session_id);
                }
            }
        }
        for entry in partial_entries {
            if finished_used_bytes <= target_used_bytes {
                break;
            }
            if evicted_session_id_set.contains(&entry.session_id) {
                continue;
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
                .remove_session_managed_resources_for_eviction(&entry.session_id)?;
            finished_used_bytes = finished_used_bytes.saturating_sub(entry.size_bytes);
            evicted_bytes = evicted_bytes.saturating_add(entry.size_bytes);
            if evicted_session_id_set.insert(entry.session_id.clone()) {
                evicted_session_ids.push(entry.session_id);
            }
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
        if self.completed_hls_cache_entry_matches_completed_task(entry) {
            return true;
        }
        if self.completed_hls_cache_entry_belongs_to_completed_task(entry)
            && self
                .tasks
                .completed_playback_task_for_hls_session(&entry.session_id)
                .is_none()
        {
            return true;
        }

        let Some(task) = self
            .tasks
            .completed_playback_task_for_hls_session(&entry.session_id)
        else {
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

    fn completed_hls_cache_entry_matches_completed_task(
        &self,
        entry: &HlsCacheCompletedEntry,
    ) -> bool {
        let Some(task) = self
            .tasks
            .completed_playback_task_for_any_hls_session(&entry.session_id)
        else {
            return false;
        };

        task.kind() == TaskKind::BilibiliProgressivePlayback
            && task.state() == TaskState::Completed
            && self.tasks.completed_playback_task_matches_hls_cache_item(
                &task,
                &entry.session_id,
                &entry.library_item_id,
            )
    }

    fn completed_hls_cache_entry_belongs_to_completed_task(
        &self,
        entry: &HlsCacheCompletedEntry,
    ) -> bool {
        let Some(task) = self
            .tasks
            .completed_playback_task_for_any_hls_session(&entry.session_id)
        else {
            return false;
        };

        task.kind() == TaskKind::BilibiliProgressivePlayback
            && task.state() == TaskState::Completed
            && self.completed_hls_cache_entry_belongs_to_task(&task, &entry.session_id)
    }

    fn completed_hls_cache_entry_belongs_to_task(&self, task: &Task, session_id: &str) -> bool {
        self.tasks
            .playback_hls_session_ids(&task.id)
            .iter()
            .any(|task_session_id| task_session_id == session_id)
    }

    fn completed_hls_cache_entry_is_primary_task_item(
        &self,
        task: &Task,
        session_id: &str,
        library_item_id: &str,
    ) -> bool {
        if task.library_item_id != library_item_id {
            return false;
        }
        HlsCacheStore::session_id_from_library_item_id(&task.library_item_id)
            .is_some_and(|task_session_id| task_session_id == session_id)
            || task
                .playback_session
                .as_ref()
                .is_some_and(|session| session.id == session_id)
            || task.id == session_id
    }

    fn completed_hls_cache_entry_is_completed_secondary_result_item(
        &self,
        task: &Task,
        session_id: &str,
        library_item_id: &str,
    ) -> bool {
        !self.completed_hls_cache_entry_is_primary_task_item(task, session_id, library_item_id)
            && task.result_items.iter().any(|item| {
                item.library_item_id == library_item_id
                    && item.state == i32::from(TaskState::Completed)
                    && (item.id == session_id
                        || item
                            .playback_source
                            .as_ref()
                            .is_some_and(|source| source.item_id == session_id)
                        || item
                            .playback_session
                            .as_ref()
                            .is_some_and(|session| session.id == session_id))
            })
    }

    fn remove_evicted_completed_hls_task(
        &self,
        entry: &HlsCacheCompletedEntry,
    ) -> io::Result<bool> {
        let Some((removal_session_id, removal_library_item_id)) =
            self.completed_hls_task_cleanup_item(&entry.session_id, &entry.library_item_id)
        else {
            return Ok(true);
        };
        self.tasks
            .remove_completed_playback_task(&removal_session_id, &removal_library_item_id)
            .map_err(|status| {
                io::Error::other(format!(
                    "failed to persist HLS playback task removal before evicting {}: {status}",
                    entry.session_id
                ))
            })
    }

    pub fn spawn_hls_cache_quota_monitor(&self) -> Option<JoinHandle<()>> {
        Some(self.spawn_hls_cache_quota_monitor_at_interval(HLS_CACHE_EVICTION_CHECK_INTERVAL))
    }

    fn spawn_hls_cache_quota_monitor_at_interval(
        &self,
        check_interval: Duration,
    ) -> JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            let start = tokio::time::Instant::now() + check_interval;
            let mut interval = tokio::time::interval_at(start, check_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(error) = state.enforce_hls_cache_quota("periodic", Vec::new(), 0) {
                    eprintln!("Failed to run periodic HLS cache eviction: {error}");
                }
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn spawn_hls_cache_quota_monitor_for_tests(
        &self,
        check_interval: Duration,
    ) -> JoinHandle<()> {
        self.spawn_hls_cache_quota_monitor_at_interval(check_interval)
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
            if self.completed_hls_task_is_authorized(session_id) {
                return self.ensure_completed_hls_session_registered(session_id);
            }
            if !self.tasks.is_playback_result_session_playable(
                session_id,
                self.supports_completed_hls_cache_playback(),
            ) {
                return false;
            }
            let Some(session) = self.hls_cache.playback_session(session_id) else {
                return false;
            };
            self.register_hls_playback_session(session);
            return true;
        };
        if task.kind() != TaskKind::BilibiliProgressivePlayback {
            return false;
        }

        match task.state() {
            TaskState::Playable => {
                if !self
                    .tasks
                    .is_hls_session_playable_for_task(&task.id, session_id)
                {
                    return false;
                }
                let Some(session) = self.hls_cache.playback_session(session_id) else {
                    return false;
                };
                self.register_hls_playback_session(session);
                true
            }
            TaskState::Completed => {
                if !self.supports_completed_hls_cache_playback() {
                    return false;
                }
                let library_item_id = HlsCacheStore::completed_library_item_id(session_id);
                if !self.tasks.playback_task_has_completed_hls_cache_item(
                    &task,
                    session_id,
                    &library_item_id,
                ) {
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
        self.register_hls_playback_session(sanitized_completed_session(&session));
        true
    }

    fn restored_hls_playback_source_needs_refresh(&self, session: &HlsPlaybackSession) -> bool {
        let existing_uri = self.tasks.hls_playback_source_uri(&session.id);
        let restored_uri = self
            .playback_uri_factory
            .create_hls_master_playlist_for_restored_task(&session.id, existing_uri.as_deref());
        existing_uri.as_deref() != Some(restored_uri.as_str())
    }

    fn fail_completed_hls_task_after_cache_restore(&self, session_id: &str) {
        self.remove_hls_playback_session(session_id);
        if let Err(status) = self
            .tasks
            .fail_unrestorable_playback_session_after_cache_restore(
                session_id,
                "Restored completed HLS cache item did not match the persisted playback task."
                    .to_owned(),
            )
        {
            eprintln!(
                "Failed to mark completed HLS playback task {session_id} failed after cache restore validation: {status}"
            );
        }
    }

    fn fail_unrestorable_hls_playback_session_if_cache_is_accessible(&self, session_id: &str) {
        if self.hls_cache.load_sessions().is_err() {
            return;
        }
        self.remove_hls_playback_session(session_id);
        if let Err(status) = self
            .tasks
            .fail_unrestorable_playback_session_after_cache_restore(
                session_id,
                "Restored HLS media session was missing from the cache.".to_owned(),
            )
        {
            eprintln!(
                "Failed to mark unrestorable HLS playback session {session_id} after cache restore validation: {status}"
            );
        }
    }
}

fn error_detail_for_log(credentials_configured: bool, detail: &dyn Display) -> String {
    if credentials_configured {
        CREDENTIAL_SAFE_LOG_DETAIL.to_owned()
    } else {
        detail.to_string()
    }
}

pub(crate) fn credential_safe_client_error(
    credentials_configured: bool,
    detail: &dyn Display,
) -> String {
    let detail = detail.to_string();
    if !credentials_configured {
        return detail;
    }

    let class = tagged_bilibili_failure_class(&detail)
        .unwrap_or_else(|| classify_bilibili_failure(&detail));
    format!(
        "{CREDENTIAL_SAFE_CLIENT_DETAIL} [{BILIBILI_FAILURE_CLASS_TAG}={}]",
        class.as_str()
    )
}

pub(crate) fn credential_safe_client_cancellation(
    credentials_configured: bool,
    detail: &str,
) -> String {
    if !credentials_configured
        || detail == CREDENTIAL_SAFE_CLIENT_CANCELLATION_DETAIL
        || task_registry::is_known_safe_cancellation_message(detail)
    {
        return detail.to_owned();
    }

    CREDENTIAL_SAFE_CLIENT_CANCELLATION_DETAIL.to_owned()
}

fn credential_safe_client_error_with_context(
    credentials_configured: bool,
    context: &'static str,
    detail: &dyn Display,
) -> String {
    let detail = credential_safe_client_error(credentials_configured, detail);
    if credentials_configured {
        format!("{detail} {context}.")
    } else {
        format!("{context}: {detail}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BilibiliFailureClass {
    Credential,
    EmptyAccountState,
    UpstreamSchemaOrAvailability,
    RestrictedProxy,
    ServerBug,
}

impl BilibiliFailureClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Credential => "credential",
            Self::EmptyAccountState => "empty_account_state",
            Self::UpstreamSchemaOrAvailability => "upstream_schema_or_availability",
            Self::RestrictedProxy => "restricted_proxy",
            Self::ServerBug => "server_bug",
        }
    }
}

fn tagged_bilibili_failure_class(detail: &str) -> Option<BilibiliFailureClass> {
    if !detail.starts_with(CREDENTIAL_SAFE_CLIENT_DETAIL) {
        return None;
    }
    [
        BilibiliFailureClass::Credential,
        BilibiliFailureClass::EmptyAccountState,
        BilibiliFailureClass::RestrictedProxy,
        BilibiliFailureClass::UpstreamSchemaOrAvailability,
        BilibiliFailureClass::ServerBug,
    ]
    .into_iter()
    .find(|class| {
        detail.contains(&format!(
            "[{BILIBILI_FAILURE_CLASS_TAG}={}]",
            class.as_str()
        ))
    })
}

fn classify_bilibili_failure(detail: &str) -> BilibiliFailureClass {
    let detail = untagged_bilibili_failure_detail(detail);
    if contains_bilibili_failure_marker(
        &detail,
        &[
            "area",
            "region",
            "restricted",
            "proxy",
            "\u{5730}\u{533a}",
            "\u{7248}\u{6743}",
            "\u{4e0d}\u{53ef}\u{89c2}\u{770b}",
        ],
    ) {
        return BilibiliFailureClass::RestrictedProxy;
    }
    if contains_bilibili_failure_marker(
        &detail,
        &[
            "empty account",
            "watch later is empty",
            "history is empty",
            "\u{6ca1}\u{6709}\u{66f4}\u{591a}",
        ],
    ) {
        return BilibiliFailureClass::EmptyAccountState;
    }
    if contains_bilibili_failure_marker(
        &detail,
        &[
            "credential file",
            "credential store",
            "credential profile",
            "cookie",
            "login",
            "not logged",
            "sessdata",
            "csrf",
            "unauthorized",
            "-101",
            "\u{8d26}\u{53f7}\u{672a}\u{767b}\u{5f55}",
            "\u{672a}\u{767b}\u{5f55}",
        ],
    ) {
        return BilibiliFailureClass::Credential;
    }
    if contains_bilibili_failure_marker(
        &detail,
        &[
            "upstream",
            "schema",
            "availability",
            "playurl",
            "resolve",
            "selected bilibili item",
            "selected collection item",
            "was not found",
            "no longer matches",
            "failed to fetch",
            "request failed",
            "network",
            "connection",
            "timed out",
            "timeout",
            "temporarily unavailable",
            "http status",
            "missing field",
            "stream reset",
        ],
    ) {
        return BilibiliFailureClass::UpstreamSchemaOrAvailability;
    }
    BilibiliFailureClass::ServerBug
}

fn untagged_bilibili_failure_detail(detail: &str) -> String {
    let mut detail = detail.to_ascii_lowercase();
    for class in [
        BilibiliFailureClass::Credential,
        BilibiliFailureClass::EmptyAccountState,
        BilibiliFailureClass::RestrictedProxy,
        BilibiliFailureClass::UpstreamSchemaOrAvailability,
        BilibiliFailureClass::ServerBug,
    ] {
        detail = detail.replace(
            &format!("[{BILIBILI_FAILURE_CLASS_TAG}={}]", class.as_str()),
            "",
        );
    }
    detail
}

fn contains_bilibili_failure_marker(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn refresh_restored_hls_playback_source(
    tasks: &BilibiliTaskRegistry,
    playback_uri_factory: &PlaybackUriFactory,
    session: &crate::hls::HlsPlaybackSession,
    completed_session_ids: &HashSet<String>,
) {
    refresh_restored_hls_playback_source_for_session(
        tasks,
        playback_uri_factory,
        session,
        completed_session_ids.contains(&session.id),
    );
}

fn refresh_restored_hls_playback_source_for_session(
    tasks: &BilibiliTaskRegistry,
    playback_uri_factory: &PlaybackUriFactory,
    session: &crate::hls::HlsPlaybackSession,
    is_completed_session: bool,
) {
    let item_id = if is_completed_session {
        HlsCacheStore::completed_library_item_id(&session.id)
    } else {
        session.id.clone()
    };
    let existing_uri = tasks.hls_playback_source_uri(&session.id);
    let playback_source = PlaybackSource {
        item_id,
        variant_id: session.variant.id.clone(),
        protocol: PlaybackProtocol::Hls.into(),
        uri: playback_uri_factory
            .create_hls_master_playlist_for_restored_task(&session.id, existing_uri.as_deref()),
        expires_at: None,
    };
    let playback_session =
        is_completed_session.then(|| playback_session_from_hls_cache_session(session));

    if let Err(status) = tasks.refresh_hls_playback_source_with_metadata(
        &session.id,
        playback_source,
        playback_session,
    ) {
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
    completed_hls_cache_playback_supported: bool,
) -> bool {
    let Ok(task) = tasks.get_task(session_id) else {
        return tasks.is_playback_result_session_playable(
            session_id,
            completed_hls_cache_playback_supported,
        );
    };
    if task.kind() != TaskKind::BilibiliProgressivePlayback {
        return false;
    }

    match task.state() {
        TaskState::Playable => tasks.is_hls_session_playable_for_task(&task.id, session_id),
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
            "/resources/{resource_id}",
            get(resource_get).head(resource_head),
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
        future::Future,
        io,
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
        pin::Pin,
        thread,
        time::Instant,
    };

    use super::*;
    use crate::{
        bbdown_adapter::{
            BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest,
            BilibiliMediaRequestKind,
        },
        bilibili_playback::{BilibiliPlaybackPlanner, BilibiliPlaybackPlanningRequest},
        bilibili_worker::BilibiliDownloadError,
        generated::tvos_net_player::v1::{BilibiliPlaybackSession, BilibiliPlaybackVariant},
        hls::{HlsAbrMetadata, HlsMediaResource, HlsVariant},
        transcoding::HlsTranscodingPlan,
    };

    #[test]
    fn credential_safe_log_detail_omits_raw_upstream_error() {
        let detail = "https://example.test/video.m4s?access_key=credential-sensitive-marker";

        assert_eq!(detail, error_detail_for_log(false, &detail));
        let safe = error_detail_for_log(true, &detail);
        assert_eq!(CREDENTIAL_SAFE_LOG_DETAIL, safe);
        assert!(!safe.contains("credential-sensitive-marker"));
    }

    #[test]
    fn credential_safe_client_error_omits_raw_upstream_error() {
        let detail = "https://example.test/video.m4s?access_key=credential-sensitive-marker";

        assert_eq!(detail, credential_safe_client_error(false, &detail));
        let safe = credential_safe_client_error(true, &detail);
        assert!(safe.starts_with(CREDENTIAL_SAFE_CLIENT_DETAIL));
        assert!(safe.contains("[bilibili_failure_class=server_bug]"));
        assert!(!safe.contains("credential-sensitive-marker"));
    }

    #[test]
    fn credential_safe_client_error_preserves_only_existing_failure_class() {
        let detail = format!(
            "{CREDENTIAL_SAFE_CLIENT_DETAIL} [bilibili_failure_class=restricted_proxy] appended-sensitive-marker"
        );

        let safe = credential_safe_client_error(true, &detail);

        assert_eq!(
            format!("{CREDENTIAL_SAFE_CLIENT_DETAIL} [bilibili_failure_class=restricted_proxy]"),
            safe
        );
        assert!(!safe.contains("appended-sensitive-marker"));
    }

    #[test]
    fn credential_safe_client_error_does_not_trust_raw_failure_class_tag() {
        let detail =
            "upstream request failed [bilibili_failure_class=credential] raw-sensitive-marker";

        let safe = credential_safe_client_error(true, &detail);

        assert!(safe.contains("[bilibili_failure_class=upstream_schema_or_availability]"));
        assert!(!safe.contains("raw-sensitive-marker"));
    }

    #[test]
    fn credential_safe_client_cancellation_preserves_only_server_owned_detail() {
        // Synthetic token fixture: joey-private-v3/access-a.
        let synthetic_access_token = "codex_synth_v1_access_a";
        assert_eq!(
            "Cancelled before playback planning started.",
            credential_safe_client_cancellation(
                true,
                "Cancelled before playback planning started."
            )
        );
        assert_eq!(
            CREDENTIAL_SAFE_CLIENT_CANCELLATION_DETAIL,
            credential_safe_client_cancellation(
                true,
                &format!(
                    "Cancelled after upstream request https://example.test/media?access_key={synthetic_access_token}"
                )
            )
        );
        assert_eq!(
            "adapter cancellation detail",
            credential_safe_client_cancellation(false, "adapter cancellation detail")
        );
    }

    #[test]
    fn credential_safe_client_error_context_survives_boundary_redaction() {
        assert_eq!(
            "Playable online; offline cache fill failed: upstream request failed",
            credential_safe_client_error_with_context(
                false,
                "Playable online; offline cache fill failed",
                &"upstream request failed",
            )
        );
        for context in [
            "Playable online; offline cache fill failed",
            "Failed to restore offline HLS cache after restart",
        ] {
            let wrapped = credential_safe_client_error_with_context(
                true,
                context,
                &"restricted proxy response-sensitive-marker",
            );

            assert!(wrapped.starts_with(CREDENTIAL_SAFE_CLIENT_DETAIL));
            assert!(wrapped.contains("[bilibili_failure_class=restricted_proxy]"));
            assert!(wrapped.contains(context));
            assert!(!wrapped.contains("response-sensitive-marker"));
            assert_eq!(
                format!(
                    "{CREDENTIAL_SAFE_CLIENT_DETAIL} [bilibili_failure_class=restricted_proxy]"
                ),
                credential_safe_client_error(true, &wrapped)
            );
        }
    }

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
    async fn malformed_task_snapshot_blocks_raw_completed_hls_deletion() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf());
        let response_body = axum::body::Bytes::from(test_fake_mp4());
        let response_size = response_body.len() as u64;
        let upstream = Router::new().route(
            "/video.m4s",
            get(move || {
                let response_body = response_body.clone();
                async move { response_body }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test upstream should bind");
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream)
                .await
                .expect("test upstream should run");
        });
        let session_id = "malformed-state-raw-hls";
        let mut session = sample_hls_session(session_id);
        session.variant.video.request.url = format!("http://{upstream_addr}/video.m4s");
        session.variant.video.request.size = Some(response_size);
        let hls_cache = HlsCacheStore::new(root_path.clone());
        let item_id = hls_cache
            .cache_session_resources(&reqwest::Client::new(), &session)
            .await
            .expect("raw completed HLS item should be cached");
        let task_state_path = root_path.join(".state").join("tasks.json");
        std::fs::create_dir_all(task_state_path.parent().unwrap())
            .expect("task state directory should be created");
        std::fs::write(&task_state_path, b"{ malformed task snapshot")
            .expect("malformed task snapshot should be written");

        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: task_state_path.clone(),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(NoopPlaybackPlanner),
        );
        assert!(state.tasks.persistence_configured());
        assert!(!state.tasks.persistence_available());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&item_id)
                .is_some()
        );

        let error = state
            .delete_completed_hls_library_item(&item_id)
            .expect_err("raw HLS deletion must fail while configured persistence is unavailable");

        assert_eq!(tonic::Code::Unavailable, error.code());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&item_id)
                .is_some()
        );
        assert_eq!(
            b"{ malformed task snapshot",
            std::fs::read(&task_state_path)
                .expect("malformed task snapshot should be preserved")
                .as_slice()
        );
        upstream_task.abort();
    }

    #[tokio::test]
    async fn manual_hls_deletion_waits_for_the_quota_snapshot_lock() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf());
        let response_body = axum::body::Bytes::from(test_fake_mp4());
        let response_size = response_body.len() as u64;
        let upstream = Router::new().route(
            "/video.m4s",
            get(move || {
                let response_body = response_body.clone();
                async move { response_body }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test upstream should bind");
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream)
                .await
                .expect("test upstream should run");
        });
        let session_id = "manual-delete-quota-lock";
        let mut session = sample_hls_session(session_id);
        session.variant.video.request.url = format!("http://{upstream_addr}/video.m4s");
        session.variant.video.request.size = Some(response_size);
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(NoopPlaybackPlanner),
        );
        let item_id = state
            .hls_cache
            .cache_session_resources(&reqwest::Client::new(), &session)
            .await
            .expect("raw completed HLS item should be cached");
        let quota_guard = state
            .hls_cache_quota_enforcement_lock
            .lock()
            .expect("quota lock should be acquired for test");
        let delete_state = state.clone();
        let delete_item_id = item_id.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let deletion = thread::spawn(move || {
            started_tx
                .send(())
                .expect("test should observe deletion start");
            let result = delete_state.delete_completed_hls_library_item(&delete_item_id);
            finished_tx
                .send(result)
                .expect("test should observe deletion result");
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("deletion thread should start");

        assert!(
            finished_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "manual deletion must wait until the quota snapshot is released"
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&item_id)
                .is_some()
        );

        drop(quota_guard);
        assert_eq!(
            Some(true),
            finished_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("deletion should finish after quota unlock")
                .expect("manual deletion should succeed")
        );
        deletion.join().expect("deletion thread should not panic");
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&item_id)
                .is_none()
        );
        upstream_task.abort();
    }

    #[tokio::test]
    async fn completed_runtime_session_scrubs_alternate_upstream_after_grace_period() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        let mut session = sample_hls_session("completion-grace");
        let mut alternate = session.variant.clone();
        alternate.id = "h264-alternate".to_owned();
        alternate.video.id = "alternate-video.m4s".to_owned();
        alternate.video.request.url = "https://example.test/alternate-video.m4s".to_owned();
        session.alternate_variants = vec![alternate];

        state
            .register_completed_hls_runtime_session_with_grace(&session, Duration::from_millis(10));

        let runtime = state
            .hls_sessions
            .get(&session.id)
            .expect("completed runtime session should be registered");
        assert!(runtime.variant.video.request.url.is_empty());
        assert_eq!(
            "https://example.test/alternate-video.m4s",
            runtime.alternate_variants[0].video.request.url
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        let scrubbed = state
            .hls_sessions
            .get(&session.id)
            .expect("completed runtime session should remain registered");
        assert!(scrubbed.variant.video.request.url.is_empty());
        assert!(scrubbed.alternate_variants[0].video.request.url.is_empty());
        assert!(
            scrubbed.alternate_variants[0]
                .video
                .request
                .headers
                .is_empty()
        );
    }

    #[test]
    fn completed_runtime_session_enforces_expired_grace_during_lookup() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        let mut session = sample_hls_session("completion-expired-lookup");
        let mut alternate = session.variant.clone();
        alternate.id = "h264-alternate".to_owned();
        alternate.video.id = "alternate-video.m4s".to_owned();
        alternate.video.request.url = "https://example.test/alternate-video.m4s".to_owned();
        alternate
            .video
            .request
            .headers
            .push(crate::bbdown_adapter::BilibiliHttpHeader {
                name: "Authorization".to_owned(),
                value: "credential-sensitive-marker".to_owned(),
            });
        session.alternate_variants = vec![alternate];

        state.hls_sessions.insert_with_scrub_deadline(
            completed_runtime_session(&session),
            sanitized_completed_session(&session),
            MonotonicInstant::now()
                .checked_sub(Duration::from_secs(1))
                .expect("expired monotonic deadline should fit"),
        );

        let served = state
            .hls_playback_session(&session.id)
            .expect("completed runtime session should remain registered");
        assert!(served.alternate_variants[0].video.request.url.is_empty());
        assert!(
            served.alternate_variants[0]
                .video
                .request
                .headers
                .is_empty()
        );
    }

    #[test]
    fn completed_runtime_timer_scrub_does_not_recheck_deadline() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        let mut session = sample_hls_session("completion-timer-scrub");
        let mut alternate = session.variant.clone();
        alternate.id = "h264-alternate".to_owned();
        alternate.video.id = "alternate-video.m4s".to_owned();
        alternate.video.request.url = "https://example.test/alternate-video.m4s".to_owned();
        session.alternate_variants = vec![alternate];

        let generation = state.hls_sessions.insert_with_scrub_deadline(
            completed_runtime_session(&session),
            sanitized_completed_session(&session),
            MonotonicInstant::now()
                .checked_add(Duration::from_secs(60))
                .expect("future monotonic deadline should fit"),
        );

        assert!(state.hls_sessions.scrub_generation(&session.id, generation));
        let scrubbed = state
            .hls_sessions
            .get(&session.id)
            .expect("completed runtime session should remain registered");
        assert!(scrubbed.alternate_variants[0].video.request.url.is_empty());
    }

    #[tokio::test]
    async fn completed_runtime_scrub_does_not_replace_newer_session() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        let mut completed = sample_hls_session("completion-replaced");
        let mut alternate = completed.variant.clone();
        alternate.id = "h264-alternate".to_owned();
        alternate.video.id = "alternate-video.m4s".to_owned();
        completed.alternate_variants = vec![alternate];
        state.register_completed_hls_runtime_session_with_grace(
            &completed,
            Duration::from_millis(10),
        );

        let newer = completed_runtime_session(&completed);
        state.hls_sessions.insert(newer.clone());

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(Some(newer), state.hls_sessions.get(&completed.id));
    }

    #[tokio::test]
    async fn background_work_idle_tracks_pending_and_active_work() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        assert!(state.background_work_is_idle());

        let planning_activity = state.begin_playback_planning();
        assert!(!state.background_work_is_idle());
        drop(planning_activity);
        assert!(state.background_work_is_idle());

        let planning_permit = Arc::clone(&state.playback_planning_permits)
            .acquire_owned()
            .await
            .expect("planning permit should be available");
        assert!(!state.background_work_is_idle());
        drop(planning_permit);

        let finalization_permit = Arc::clone(&state.hls_cache_finalization_permits)
            .acquire_owned()
            .await
            .expect("finalization permit should be available");
        assert!(!state.background_work_is_idle());
        drop(finalization_permit);

        let transcoding_permit = Arc::clone(&state.lan_transcoding_permits)
            .acquire_owned()
            .await
            .expect("transcoding permit should be available");
        assert!(!state.background_work_is_idle());
        drop(transcoding_permit);

        state
            .lan_transcoding_active_jobs
            .fetch_add(1, Ordering::SeqCst);
        assert!(!state.background_work_is_idle());
        state
            .lan_transcoding_active_jobs
            .fetch_sub(1, Ordering::SeqCst);

        let (task_id, session) = create_playable_hls_task(&state, "BV1background-idle");
        assert!(state.hls_fill_scheduler.enqueue_foreground(
            task_id.clone(),
            session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(!state.background_work_is_idle());
        let current_job = state.hls_fill_scheduler.next_job().await;
        assert!(!state.background_work_is_idle());
        state.cancel_hls_fill_work_for_task(&task_id);
        assert!(current_job.token.is_cancelled());
        state.hls_fill_scheduler.finish_current(&current_job, false);
        assert!(state.background_work_is_idle());
    }

    #[tokio::test]
    async fn app_state_shutdown_stops_idle_hls_fill_worker() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        state.enqueue_hls_cache_fill_foreground(
            "missing-task".to_owned(),
            sample_hls_session("shutdown-worker"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while !state.hls_fill_scheduler.is_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("HLS fill worker should finish the rejected job");
        assert!(state.hls_fill_scheduler.worker_started_for_tests());

        tokio::time::timeout(Duration::from_secs(1), state.shutdown_hls_fill_worker())
            .await
            .expect("idle HLS fill worker should stop after shutdown");
        assert!(!state.hls_fill_scheduler.worker_started_for_tests());
    }

    #[tokio::test]
    async fn playback_progress_promotes_before_waiting_for_quota_lock() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        let (current_task_id, current_session) =
            create_playable_hls_task(&state, "BV1quota-current");
        let (active_task_id, active_session) = create_playable_hls_task(&state, "BV1quota-active");
        assert!(state.hls_fill_scheduler.enqueue_foreground(
            current_task_id,
            current_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let current_job = state.hls_fill_scheduler.next_job().await;
        assert!(!state.hls_fill_scheduler.enqueue_demoted(
            active_task_id.clone(),
            active_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));

        let quota_lock = state
            .hls_cache_quota_enforcement_lock
            .lock()
            .expect("quota lock should be acquired for test");
        let report_state = state.clone();
        let report_task_id = active_task_id.clone();
        let report_handle = thread::spawn(move || {
            report_state.record_hls_playback_progress(PlaybackProgressReport {
                playback_uri: format!(
                    "http://media.example.test:8080/hls/{report_task_id}/master.m3u8"
                ),
                library_item_id: String::new(),
                variant_id: "h264".to_owned(),
                position_seconds: 42.0,
                duration_seconds: Some(120.0),
                intent: crate::hls_playback_progress::PlaybackProgressIntent::Seek,
                reported_at: SystemTime::now(),
            })
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut preempted_before_quota_unlock = current_job.token.is_preempted();
        while !preempted_before_quota_unlock && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
            preempted_before_quota_unlock = current_job.token.is_preempted();
        }
        assert!(
            preempted_before_quota_unlock,
            "playback progress should preempt before waiting for quota lock"
        );
        let snapshot = state
            .hls_playback_progress_for_session(&active_task_id)
            .expect("reported playback progress should be visible before quota unlock");
        assert_eq!(42.0, snapshot.position_seconds);
        drop(quota_lock);
        let outcome = report_handle
            .join()
            .expect("playback progress thread should not panic");

        assert!(outcome.accepted);
        assert_eq!(active_task_id, outcome.session_id);
    }

    #[tokio::test]
    async fn playback_progress_heartbeat_keeps_current_hls_fill_running() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        let (task_id, session) = create_playable_hls_task(&state, "BV1heartbeat");
        assert!(state.hls_fill_scheduler.enqueue_foreground(
            task_id.clone(),
            session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let current_job = state.hls_fill_scheduler.next_job().await;

        let outcome = state.record_hls_playback_progress(PlaybackProgressReport {
            playback_uri: format!("http://media.example.test:8080/hls/{task_id}/master.m3u8"),
            library_item_id: String::new(),
            variant_id: "h264".to_owned(),
            position_seconds: 42.0,
            duration_seconds: Some(120.0),
            intent: PlaybackProgressIntent::Playing,
            reported_at: SystemTime::now(),
        });

        assert!(outcome.accepted);
        assert_eq!(task_id, outcome.session_id);
        assert!(!current_job.token.is_preempted());
        assert_eq!(
            0,
            state
                .hls_fill_scheduler
                .queued_session_count_for_tests(&outcome.session_id)
        );
    }

    #[tokio::test]
    async fn paused_playback_progress_keeps_cache_recent_without_promoting_fill() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state = test_app_state(&temp);
        let (current_task_id, current_session) =
            create_playable_hls_task(&state, "BV1paused-current");
        let (paused_task_id, paused_session) =
            create_playable_hls_task(&state, "BV1paused-session");
        assert!(state.hls_fill_scheduler.enqueue_foreground(
            current_task_id,
            current_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let current_job = state.hls_fill_scheduler.next_job().await;
        assert!(!state.hls_fill_scheduler.enqueue_demoted(
            paused_task_id.clone(),
            paused_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));

        let outcome = state.record_hls_playback_progress(PlaybackProgressReport {
            playback_uri: format!(
                "http://media.example.test:8080/hls/{paused_task_id}/master.m3u8"
            ),
            library_item_id: String::new(),
            variant_id: "h264".to_owned(),
            position_seconds: 42.0,
            duration_seconds: Some(120.0),
            intent: PlaybackProgressIntent::Paused,
            reported_at: SystemTime::now(),
        });

        assert!(outcome.accepted);
        assert_eq!(paused_task_id, outcome.session_id);
        assert!(!current_job.token.is_preempted());
        assert_eq!(
            1,
            state
                .hls_fill_scheduler
                .queued_session_count_for_tests(&outcome.session_id)
        );
        assert!(
            state
                .recently_used_hls_cache_session_ids()
                .contains(&outcome.session_id)
        );
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

    type TestPlanningFuture<'a> = Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::bbdown_adapter::BilibiliPlaybackPlan,
                        BilibiliDownloadError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    struct NoopPlaybackPlanner;

    impl BilibiliPlaybackPlanner for NoopPlaybackPlanner {
        fn plan<'a>(&'a self, _request: BilibiliPlaybackPlanningRequest) -> TestPlanningFuture<'a> {
            Box::pin(async {
                Err(BilibiliDownloadError::Failed(
                    "test planner is not configured".to_owned(),
                ))
            })
        }
    }

    fn test_app_state(temp: &tempfile::TempDir) -> AppState {
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf());
        AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(NoopPlaybackPlanner),
        )
    }

    fn create_playable_hls_task(state: &AppState, source: &str) -> (String, HlsPlaybackSession) {
        let creation = state
            .tasks
            .create_bilibili_playback_task(source, None, None)
            .expect("playback task should be created");
        let task_id = creation.task.id;
        let session = sample_hls_session(&task_id);
        state
            .hls_cache
            .save_session(&session)
            .expect("HLS session should persist");
        state.hls_sessions.insert(session.clone());
        state
            .tasks
            .complete_playback_playable(
                &task_id,
                session.title.clone(),
                PlaybackSource {
                    item_id: task_id.clone(),
                    variant_id: session.variant.id.clone(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: format!("http://media.example.test:8080/hls/{task_id}/master.m3u8"),
                    expires_at: None,
                },
                sample_playback_session(&task_id),
            )
            .expect("task should become playable");
        (task_id, session)
    }

    fn sample_playback_session(session_id: &str) -> BilibiliPlaybackSession {
        BilibiliPlaybackSession {
            id: session_id.to_owned(),
            title: "Episode".to_owned(),
            content_id: "cid-1".to_owned(),
            selected_variant_id: "h264".to_owned(),
            selected_variant: Some(BilibiliPlaybackVariant {
                id: "h264".to_owned(),
                label: "1920x1080".to_owned(),
                source_kind: "dash".to_owned(),
                container: "mp4".to_owned(),
                video_codec: "avc1.640028".to_owned(),
                audio_codec: String::new(),
                width: 1920,
                height: 1080,
                bitrate: 1_000_000,
                size_bytes: 1024,
            }),
            variants: Vec::new(),
            transcoding_plan: None,
            effective_policy: Some(crate::playback_policy::PlaybackPolicy::default().to_proto()),
        }
    }

    fn sample_hls_session(session_id: &str) -> HlsPlaybackSession {
        HlsPlaybackSession {
            id: session_id.to_owned(),
            title: "Episode".to_owned(),
            variant: HlsVariant {
                id: "h264".to_owned(),
                bandwidth: 1_000_000,
                codecs: vec!["avc1.640028".to_owned()],
                width: Some(1920),
                height: Some(1080),
                duration_seconds: 120,
                video: HlsMediaResource {
                    id: "video.m4s".to_owned(),
                    request: BilibiliMediaRequest {
                        kind: BilibiliMediaRequestKind::Video,
                        stream_id: None,
                        url: "https://example.test/video.m4s".to_owned(),
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
                        size: Some(1024),
                        duration_seconds: Some(120),
                        cache_key: BilibiliMediaCacheKey {
                            content_id: "cid-1".to_owned(),
                            media_kind: BilibiliMediaRequestKind::Video,
                            stream_id: None,
                            codecs: Some("avc1.640028".to_owned()),
                            source_hash: session_id.to_owned(),
                        },
                    },
                },
                audio: None,
            },
            alternate_variants: Vec::new(),
            advertise_alternate_variants: true,
            abr: HlsAbrMetadata::default(),
            variants: Vec::new(),
            transcoding: HlsTranscodingPlan::default(),
            effective_policy: crate::playback_policy::PlaybackPolicy::default(),
        }
    }

    fn test_fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(test_mp4_box(*b"ftyp", b"isom"));
        bytes.extend(test_mp4_box(*b"moov", b"metadata"));
        bytes.extend(test_mp4_box(*b"moof", b"frag"));
        bytes.extend(test_mp4_box(*b"mdat", b"media-data"));
        bytes
    }

    fn test_mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).expect("test MP4 box should fit");
        let mut bytes = Vec::with_capacity(size as usize);
        bytes.extend(size.to_be_bytes());
        bytes.extend(kind);
        bytes.extend(payload);
        bytes
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
