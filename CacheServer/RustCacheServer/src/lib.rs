mod bbdown_adapter;
mod bilibili_playback;
pub mod bilibili_worker;
pub mod config;
pub mod generated;
pub mod grpc_services;
mod hls;
mod hls_cache;
pub mod library;
pub mod media;
pub mod playback;
pub mod task_registry;
mod task_store;

use std::{collections::HashSet, io, net::SocketAddr, sync::Arc, time::Duration};

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
    hls_cache::{HlsCacheStore, sanitized_completed_session},
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
    pub(crate) completed_hls_cache_playback_supported: bool,
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
            completed_hls_cache_playback_supported,
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
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };

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
                        let _ = self.tasks.complete_playback_cached(
                            &session.id,
                            HlsCacheStore::completed_library_item_id(&session.id),
                        );
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

            handle.spawn(crate::grpc_services::run_hls_cache_finalization(
                self.clone(),
                session.id.clone(),
                session.clone(),
                HlsCacheFinalizationFailureMode::FailRestoredTask,
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
        if !self.supports_completed_hls_cache_playback() {
            return None;
        }
        let session_id = HlsCacheStore::session_id_from_library_item_id(item_id)?;
        if !self.completed_hls_task_is_authorized(&session_id) {
            return None;
        }
        self.hls_cache
            .create_playback_source(item_id, variant_id, uri)
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
    let grpc_state = state.clone();
    let media_state = state.clone();

    let grpc_server = run_grpc_servers(grpc_addrs, grpc_state);
    let media_server = run_media_servers(media_addrs, media_state);
    let _bilibili_worker_task = state.spawn_configured_bilibili_task_worker();

    tokio::select! {
        result = grpc_server => result,
        result = media_server => result,
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

async fn run_grpc_listener(
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

async fn run_media_listener(
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
    let mut servers = JoinSet::new();
    for listener in listeners {
        let state = state.clone();
        servers.spawn(async move { run_one(listener, state).await });
    }

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
