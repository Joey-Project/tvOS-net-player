use std::{pin::Pin, sync::Arc, time::Duration};

use futures_core::Stream;
use tokio::{sync::mpsc, time::sleep};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::{
    AppState,
    bbdown_adapter::{
        BilibiliPlaybackPlan, BilibiliPlaybackVariant as AdapterPlaybackVariant,
        BilibiliPlaybackVariantKind,
    },
    bilibili_playback::BilibiliPlaybackPlanningRequest,
    bilibili_worker::BilibiliDownloadError,
    generated::tvos_net_player::v1::{
        BilibiliPlaybackOptions, BilibiliPlaybackSession, BilibiliPlaybackVariant, CacheRoot,
        CancelTaskRequest, CheckHealthRequest, CreateBilibiliPlaybackTaskRequest,
        CreateBilibiliTaskRequest, DeleteLibraryItemRequest, DeleteLibraryItemResponse,
        GetLibraryItemRequest, GetPlaybackSourceRequest, GetServerInfoRequest, GetTaskRequest,
        HealthState, HealthStatus, LibraryItem, ListCacheRootsRequest, ListCacheRootsResponse,
        ListLibraryItemsRequest, ListLibraryItemsResponse, PlaybackProtocol, PlaybackSource,
        RescanLibraryRequest, RescanLibraryResponse, ServerCapability, ServerInfo, Task, TaskEvent,
        TaskKind, TaskState, WatchTasksRequest, cache_service_server::CacheService,
        library_service_server::LibraryService, server_service_server::ServerService,
        task_service_server::TaskService,
    },
    hls::HlsPlaybackSession,
    library::ROOT_ID,
    task_registry::{BilibiliTaskRegistry, current_timestamp},
};

const PLAYBACK_PLANNING_INTERRUPTED_MESSAGE: &str =
    "Playback planning was interrupted before it completed.";

#[derive(Clone)]
pub struct ServerGrpcService {
    state: AppState,
}

impl ServerGrpcService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl ServerService for ServerGrpcService {
    async fn get_server_info(
        &self,
        _request: Request<GetServerInfoRequest>,
    ) -> Result<Response<ServerInfo>, Status> {
        let mut info = ServerInfo {
            id: self.state.options.server_id.clone(),
            name: self.state.options.server_name.clone(),
            version: "0.1.0".to_owned(),
            media_base_uris: Vec::new(),
            capabilities: vec![
                ServerCapability::BilibiliTasks.into(),
                ServerCapability::Hls.into(),
            ],
        };

        if self.state.library.supports_http_range_playback() {
            info.capabilities.push(ServerCapability::HttpRange.into());
            if let Some(base_uri) = self
                .state
                .options
                .public_media_base_uri
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                info.media_base_uris.push(base_uri.to_owned());
            }
        }

        Ok(Response::new(info))
    }

    async fn check_health(
        &self,
        _request: Request<CheckHealthRequest>,
    ) -> Result<Response<HealthStatus>, Status> {
        let root_available = self.state.library.is_root_available().await;
        Ok(Response::new(HealthStatus {
            state: if root_available {
                HealthState::Serving.into()
            } else {
                HealthState::Degraded.into()
            },
            message: if root_available {
                "Cache root is available.".to_owned()
            } else {
                "Cache root is unavailable.".to_owned()
            },
            checked_at: Some(current_timestamp()),
        }))
    }
}

#[derive(Clone)]
pub struct LibraryGrpcService {
    state: AppState,
}

impl LibraryGrpcService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl LibraryService for LibraryGrpcService {
    async fn list_library_items(
        &self,
        request: Request<ListLibraryItemsRequest>,
    ) -> Result<Response<ListLibraryItemsResponse>, Status> {
        let request = request.into_inner();
        let page_size = if request.page_size <= 0 {
            50
        } else {
            request.page_size.min(200)
        };
        let page_offset = decode_page_token(&request.page_token);
        let page = self
            .state
            .library
            .list_items_page(request.filter.as_ref(), page_offset, page_size as usize)
            .await;

        Ok(Response::new(ListLibraryItemsResponse {
            items: page.items,
            next_page_token: page
                .next_page_offset
                .map(|offset| offset.to_string())
                .unwrap_or_default(),
        }))
    }

    async fn get_library_item(
        &self,
        request: Request<GetLibraryItemRequest>,
    ) -> Result<Response<LibraryItem>, Status> {
        let request = request.into_inner();
        let Some(item) = self.state.library.get_item(&request.id).await else {
            return Err(Status::not_found("Library item not found."));
        };

        Ok(Response::new(item))
    }

    async fn get_playback_source(
        &self,
        request: Request<GetPlaybackSourceRequest>,
    ) -> Result<Response<PlaybackSource>, Status> {
        if !self.state.library.supports_http_range_playback() {
            return Err(Status::failed_precondition(
                "HTTP range playback is unavailable on this platform.",
            ));
        }

        let item_id = request.get_ref().item_id.clone();
        let variant_id = request.get_ref().variant_id.clone();
        let Some(_) = self
            .state
            .library
            .get_media_file(&item_id, &variant_id)
            .await
        else {
            return Err(Status::not_found("Playback source not found."));
        };

        Ok(Response::new(PlaybackSource {
            item_id: item_id.clone(),
            variant_id: variant_id.clone(),
            protocol: PlaybackProtocol::HttpFile.into(),
            uri: self
                .state
                .playback_uri_factory
                .create(&request, &item_id, &variant_id),
            expires_at: None,
        }))
    }

    async fn rescan_library(
        &self,
        request: Request<RescanLibraryRequest>,
    ) -> Result<Response<RescanLibraryResponse>, Status> {
        let request = request.into_inner();
        if let Some(unknown_root_id) = request
            .cache_root_ids
            .iter()
            .find(|root_id| *root_id != ROOT_ID)
        {
            return Err(Status::not_found(format!(
                "Cache root not found: {unknown_root_id}."
            )));
        }

        Ok(Response::new(RescanLibraryResponse {
            discovered_item_count: self.state.library.count_items().await,
        }))
    }
}

#[derive(Clone)]
pub struct TaskGrpcService {
    state: AppState,
}

impl TaskGrpcService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl TaskService for TaskGrpcService {
    type WatchTasksStream = Pin<Box<dyn Stream<Item = Result<TaskEvent, Status>> + Send + 'static>>;

    async fn create_bilibili_task(
        &self,
        request: Request<CreateBilibiliTaskRequest>,
    ) -> Result<Response<Task>, Status> {
        let request = request.into_inner();
        let task = self
            .state
            .tasks
            .create_bilibili_task(&request.url_or_id, request.options)?;
        Ok(Response::new(task))
    }

    async fn create_bilibili_playback_task(
        &self,
        request: Request<CreateBilibiliPlaybackTaskRequest>,
    ) -> Result<Response<Task>, Status> {
        let url_or_id = request.get_ref().url_or_id.clone();
        let options = request.get_ref().options.clone();
        let creation = self
            .state
            .tasks
            .create_bilibili_playback_task(&url_or_id, options.clone())?;
        if !creation.created {
            return Ok(Response::new(creation.task));
        }

        let task_id = creation.task.id.clone();
        let playback_source_uri = self
            .state
            .playback_uri_factory
            .create_hls_master_playlist(&request, &task_id);
        let cancellation = creation
            .cancellation
            .expect("new playback task should include a planning cancellation token");
        let state = self.state.clone();
        tokio::spawn(run_bilibili_playback_planning(
            state,
            task_id,
            creation.task.source.clone(),
            options,
            playback_source_uri,
            cancellation,
        ));

        Ok(Response::new(creation.task))
    }

    async fn get_task(&self, request: Request<GetTaskRequest>) -> Result<Response<Task>, Status> {
        let request = request.into_inner();
        Ok(Response::new(self.state.tasks.get_task(&request.id)?))
    }

    async fn watch_tasks(
        &self,
        request: Request<WatchTasksRequest>,
    ) -> Result<Response<Self::WatchTasksStream>, Status> {
        let request = request.into_inner();
        let mut subscription = self.state.tasks.subscribe(&request.ids)?;
        let snapshots = subscription.snapshots().to_vec();
        let (sender, receiver) = mpsc::channel(128);
        tokio::spawn(async move {
            for task in snapshots {
                if sender
                    .send(Ok(TaskEvent { task: Some(task) }))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            loop {
                let result = tokio::select! {
                    _ = sender.closed() => return,
                    result = subscription.recv() => result,
                };

                match result {
                    Ok(task) => {
                        if sender
                            .send(Ok(TaskEvent { task: Some(task) }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(status) => {
                        let _ = sender.send(Err(status)).await;
                        return;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn cancel_task(
        &self,
        request: Request<CancelTaskRequest>,
    ) -> Result<Response<Task>, Status> {
        let request = request.into_inner();
        let task = self.state.tasks.cancel_task(&request.id)?;
        if task.kind() == TaskKind::BilibiliProgressivePlayback
            && task.state() != TaskState::Playable
        {
            self.state.hls_sessions.remove(&task.id);
        }
        Ok(Response::new(task))
    }
}

#[derive(Clone)]
pub struct CacheGrpcService {
    state: AppState,
}

impl CacheGrpcService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl CacheService for CacheGrpcService {
    async fn list_cache_roots(
        &self,
        _request: Request<ListCacheRootsRequest>,
    ) -> Result<Response<ListCacheRootsResponse>, Status> {
        let root: CacheRoot = self.state.library.cache_root().await;
        Ok(Response::new(ListCacheRootsResponse { roots: vec![root] }))
    }

    async fn delete_library_item(
        &self,
        _request: Request<DeleteLibraryItemRequest>,
    ) -> Result<Response<DeleteLibraryItemResponse>, Status> {
        Err(Status::unimplemented(
            "Cache deletion is not implemented in this slice.",
        ))
    }
}

fn decode_page_token(page_token: &str) -> i64 {
    page_token
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|offset| *offset > 0)
        .unwrap_or(0)
}

struct PlaybackPlanningCleanup {
    tasks: Arc<BilibiliTaskRegistry>,
    task_id: String,
    armed: bool,
}

impl PlaybackPlanningCleanup {
    fn new(tasks: Arc<BilibiliTaskRegistry>, task_id: String) -> Self {
        Self {
            tasks,
            task_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PlaybackPlanningCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.tasks.complete_task_failed(
                &self.task_id,
                PLAYBACK_PLANNING_INTERRUPTED_MESSAGE.to_owned(),
            );
        }
    }
}

async fn run_bilibili_playback_planning(
    state: AppState,
    task_id: String,
    source: String,
    options: Option<BilibiliPlaybackOptions>,
    playback_source_uri: String,
    cancellation: crate::task_registry::BilibiliTaskCancellation,
) {
    let mut cleanup = PlaybackPlanningCleanup::new(Arc::clone(&state.tasks), task_id.clone());
    let permit_request = Arc::clone(&state.playback_planning_permits).acquire_owned();
    tokio::pin!(permit_request);
    let _permit = loop {
        if cancellation.is_cancel_requested() {
            if state
                .tasks
                .complete_task_cancelled(
                    &task_id,
                    "Cancelled before playback planning started.".to_owned(),
                )
                .is_ok()
            {
                cleanup.disarm();
            }
            return;
        }

        tokio::select! {
            permit = &mut permit_request => {
                match permit {
                    Ok(permit) => break permit,
                    Err(_) => {
                        if state
                            .tasks
                            .complete_task_failed(
                                &task_id,
                                "Playback planning concurrency limiter is unavailable.".to_owned(),
                            )
                            .is_ok()
                        {
                            cleanup.disarm();
                        }
                        return;
                    }
                }
            }
            () = sleep(Duration::from_millis(100)) => {}
        }
    };
    if cancellation.is_cancel_requested() {
        if state
            .tasks
            .complete_task_cancelled(
                &task_id,
                "Cancelled before playback planning started.".to_owned(),
            )
            .is_ok()
        {
            cleanup.disarm();
        }
        return;
    }
    let planning_request = BilibiliPlaybackPlanningRequest {
        source,
        options,
        cancellation,
    };
    let plan = match state.playback_planner.plan(planning_request).await {
        Ok(plan) => plan,
        Err(error) => {
            if state
                .tasks
                .complete_task_failed(&task_id, playback_error_message(error))
                .is_ok()
            {
                cleanup.disarm();
            }
            return;
        }
    };
    let metadata = match playback_task_metadata(&task_id, plan) {
        Ok(metadata) => metadata,
        Err(error) => {
            if state
                .tasks
                .complete_task_failed(&task_id, error.message().to_owned())
                .is_ok()
            {
                cleanup.disarm();
            }
            return;
        }
    };

    state.hls_sessions.insert(metadata.hls_session);
    let playback_source = PlaybackSource {
        item_id: task_id.clone(),
        variant_id: metadata.playback_session.selected_variant_id.clone(),
        protocol: PlaybackProtocol::Hls.into(),
        uri: playback_source_uri,
        expires_at: None,
    };
    match state.tasks.complete_playback_playable(
        &task_id,
        metadata.title,
        playback_source,
        metadata.playback_session,
    ) {
        Ok(task) => {
            if task.state() != crate::generated::tvos_net_player::v1::TaskState::Playable {
                state.hls_sessions.remove(&task_id);
            }
            cleanup.disarm();
        }
        Err(_) => state.hls_sessions.remove(&task_id),
    }
}

struct PlaybackTaskMetadata {
    title: String,
    playback_session: BilibiliPlaybackSession,
    hls_session: HlsPlaybackSession,
}

fn playback_task_metadata(
    task_id: &str,
    plan: BilibiliPlaybackPlan,
) -> Result<PlaybackTaskMetadata, Status> {
    let entry = plan
        .entries
        .first()
        .ok_or_else(|| Status::failed_precondition("Playback plan did not include entries."))?;
    let selected = entry.selected_variant.as_ref().ok_or_else(|| {
        Status::failed_precondition("Playback plan did not include an AVPlayer-compatible variant.")
    })?;
    let title = if entry.title.trim().is_empty() {
        plan.title.clone()
    } else {
        entry.title.clone()
    };
    let selected_variant = playback_variant_from_adapter(&selected.variant);
    let hls_session = HlsPlaybackSession::from_selected_variant(task_id, &title, &selected.variant)
        .map_err(|error| Status::failed_precondition(error.to_string()))?;
    let playback_session = BilibiliPlaybackSession {
        id: task_id.to_owned(),
        title: title.clone(),
        content_id: entry.content_id.clone(),
        selected_variant_id: selected.variant.id.clone(),
        selected_variant: Some(selected_variant),
        variants: entry
            .variants
            .iter()
            .map(playback_variant_from_adapter)
            .collect(),
    };

    Ok(PlaybackTaskMetadata {
        title,
        playback_session,
        hls_session,
    })
}

fn playback_variant_from_adapter(variant: &AdapterPlaybackVariant) -> BilibiliPlaybackVariant {
    BilibiliPlaybackVariant {
        id: variant.id.clone(),
        label: playback_variant_label(variant),
        source_kind: playback_variant_kind_name(variant.kind).to_owned(),
        container: playback_variant_container(variant.kind).to_owned(),
        video_codec: variant.codecs.first().cloned().unwrap_or_default(),
        audio_codec: variant
            .audio
            .as_ref()
            .and_then(|audio| audio.codecs.clone())
            .unwrap_or_default(),
        width: variant
            .width
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i32::MAX),
        height: variant
            .height
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i32::MAX),
        bitrate: variant
            .bandwidth
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i64::MAX),
        size_bytes: playback_variant_size_bytes(variant)
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i64::MAX),
    }
}

fn playback_variant_label(variant: &AdapterPlaybackVariant) -> String {
    match (variant.width, variant.height) {
        (Some(width), Some(height)) => format!("{width}x{height}"),
        _ => variant.id.clone(),
    }
}

fn playback_variant_kind_name(kind: BilibiliPlaybackVariantKind) -> &'static str {
    match kind {
        BilibiliPlaybackVariantKind::Dash => "dash",
        BilibiliPlaybackVariantKind::Flv => "flv",
    }
}

fn playback_variant_container(kind: BilibiliPlaybackVariantKind) -> &'static str {
    match kind {
        BilibiliPlaybackVariantKind::Dash => "mp4",
        BilibiliPlaybackVariantKind::Flv => "flv",
    }
}

fn playback_variant_size_bytes(variant: &AdapterPlaybackVariant) -> Option<u64> {
    let mut total = 0_u64;
    let mut found = false;
    for request in variant
        .video
        .iter()
        .chain(variant.audio.iter())
        .chain(variant.flv_segments.iter())
    {
        if let Some(size_bytes) = request.size {
            total = total.saturating_add(size_bytes);
            found = true;
        }
    }

    found.then_some(total)
}

fn playback_error_message(error: BilibiliDownloadError) -> String {
    match error {
        BilibiliDownloadError::Failed(message) | BilibiliDownloadError::Cancelled(message) => {
            message
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use crate::{
        bbdown_adapter::{
            BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest,
            BilibiliMediaRequestKind, BilibiliPlaybackAbrMetadata, BilibiliPlaybackEntry,
            BilibiliPlaybackVariantSelection, BilibiliPlaybackVariantSelectionPolicy,
            BilibiliSelectedPlaybackVariant,
        },
        bilibili_playback::{
            BilibiliPlaybackPlanner, BilibiliPlaybackPlanningFuture,
            BilibiliPlaybackPlanningRequest,
        },
        config::CacheServerOptions,
        generated::tvos_net_player::v1::{
            BilibiliPlaybackOptions, CreateBilibiliPlaybackTaskRequest, TaskKind, TaskState,
        },
    };
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn create_bilibili_playback_task_returns_preparing_and_plans_hls_session_in_background() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let (planner, planner_started, plan_sender) = DeferredPlaybackPlanner::new();
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(planner),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = tokio::time::timeout(
            Duration::from_secs(2),
            service.create_bilibili_playback_task(Request::new(
                CreateBilibiliPlaybackTaskRequest {
                    url_or_id: "  BV1progressive  ".to_owned(),
                    options: Some(BilibiliPlaybackOptions {
                        quality_preference: "1080p".to_owned(),
                        encoding_preference: "h264".to_owned(),
                        prefer_tv_api: false,
                    }),
                },
            )),
        )
        .await
        .expect("RPC should return before the playback planner completes")
        .expect("playback task should be created")
        .into_inner();

        assert!(created.id.starts_with("bilibili-playback-"));
        assert_eq!(TaskKind::BilibiliProgressivePlayback, created.kind());
        assert_eq!(TaskState::Preparing, created.state());
        assert_eq!("BV1progressive", created.source);
        assert!(created.playback_source.is_none());
        assert!(created.playback_session.is_none());

        planner_started
            .await
            .expect("background playback planner should start");
        plan_sender
            .send(Ok(sample_playback_plan()))
            .expect("test should send playback plan");
        let task = wait_for_task_state(&tasks, &created.id, TaskState::Playable).await;

        assert_eq!("Episode 1", task.title);
        let playback_source = task
            .playback_source
            .as_ref()
            .expect("playable task should expose an HLS source");
        assert_eq!(task.id, playback_source.item_id);
        assert_eq!("h264", playback_source.variant_id);
        assert_eq!(PlaybackProtocol::Hls as i32, playback_source.protocol);
        assert_eq!(
            format!("http://media.example.test:8080/hls/{}/master.m3u8", task.id),
            playback_source.uri
        );
        let session = task
            .playback_session
            .expect("playback session should exist");
        assert_eq!(task.id, session.id);
        assert_eq!("BV1progressive-cid1", session.content_id);
        assert_eq!("h264", session.selected_variant_id);
        assert_eq!(2, session.variants.len());
        assert_eq!("dash", session.selected_variant.unwrap().source_kind);
        assert!(service.state.hls_sessions.get(&task.id).is_some());

        let cancelled = service
            .cancel_task(Request::new(CancelTaskRequest {
                id: task.id.clone(),
            }))
            .await
            .expect("playable playback task should cancel")
            .into_inner();

        assert_eq!(TaskState::Cancelled, cancelled.state());
        assert!(service.state.hls_sessions.get(&task.id).is_none());
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_marks_metadata_failure_terminal() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);
        let first_created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1empty".to_owned(),
                options: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();
        let first = wait_for_task_state(&tasks, &first_created.id, TaskState::Failed).await;
        let second_created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1empty".to_owned(),
                options: None,
            }))
            .await
            .expect("failed planning should allow retry")
            .into_inner();
        let second = wait_for_task_state(&tasks, &second_created.id, TaskState::Failed).await;

        assert_eq!(TaskState::Failed, first.state());
        assert_eq!("Playback plan did not include entries.", first.message);
        assert!(first.playback_source.is_none());
        assert_ne!(first.id, second.id);
        assert_eq!(TaskState::Failed, second.state());
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_returns_while_planner_is_pending() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let (planner, planner_started, plan_sender) = DeferredPlaybackPlanner::new();
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(planner),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = tokio::time::timeout(Duration::from_millis(200), async {
            service
                .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                    url_or_id: "BV1pending".to_owned(),
                    options: None,
                }))
                .await
        })
        .await
        .expect("RPC should not wait for the pending planner")
        .expect("playback task should be created")
        .into_inner();

        assert_eq!(TaskState::Preparing, created.state());
        assert_eq!(
            TaskState::Preparing,
            tasks.get_task(&created.id).unwrap().state()
        );
        planner_started
            .await
            .expect("background planner should start");

        plan_sender
            .send(Ok(sample_playback_plan()))
            .expect("test should send playback plan");
        let playable = wait_for_task_state(&tasks, &created.id, TaskState::Playable).await;
        assert_eq!(TaskState::Playable, playable.state());
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_does_not_dedupe_pending_request_scoped_tasks() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(PendingPlaybackPlanner),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let first = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1pending-duplicate".to_owned(),
                options: None,
            }))
            .await
            .expect("first playback task should be created")
            .into_inner();
        let second = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1pending-duplicate".to_owned(),
                options: None,
            }))
            .await
            .expect("second playback task should be created")
            .into_inner();

        assert_eq!(TaskState::Preparing, first.state());
        assert_eq!(TaskState::Preparing, second.state());
        assert_ne!(first.id, second.id);

        tasks
            .cancel_task(&first.id)
            .expect("first pending task should be cancellable");
        tasks
            .cancel_task(&second.id)
            .expect("second pending task should be cancellable");
        wait_for_task_state(&tasks, &first.id, TaskState::Cancelled).await;
        wait_for_task_state(&tasks, &second.id, TaskState::Cancelled).await;
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_limits_background_planning_concurrency() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let (planner, mut starts, mut results) =
            SourceControlledPlaybackPlanner::new(["BV1first", "BV1second"]);
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(planner),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);
        let first_started = starts
            .remove("BV1first")
            .expect("first start signal should exist");
        let mut second_started = starts
            .remove("BV1second")
            .expect("second start signal should exist");
        let first_result = results
            .remove("BV1first")
            .expect("first result sender should exist");
        let second_result = results
            .remove("BV1second")
            .expect("second result sender should exist");

        let first = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1first".to_owned(),
                options: None,
            }))
            .await
            .expect("first playback task should be created")
            .into_inner();
        first_started
            .await
            .expect("first background planner should start");
        let second = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1second".to_owned(),
                options: None,
            }))
            .await
            .expect("second playback task should be created")
            .into_inner();

        assert_eq!(TaskState::Preparing, first.state());
        assert_eq!(TaskState::Preparing, second.state());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut second_started)
                .await
                .is_err(),
            "second planner should wait for the global planning permit"
        );

        first_result
            .send(Ok(sample_playback_plan()))
            .expect("first plan should be delivered");
        wait_for_task_state(&tasks, &first.id, TaskState::Playable).await;
        second_started
            .await
            .expect("second planner should start after first completes");
        second_result
            .send(Ok(sample_playback_plan()))
            .expect("second plan should be delivered");
        wait_for_task_state(&tasks, &second.id, TaskState::Playable).await;
    }

    #[tokio::test]
    async fn cancelling_playback_task_waiting_for_planning_permit_finishes_without_planner_start() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let (planner, mut starts, mut results) =
            SourceControlledPlaybackPlanner::new(["BV1first", "BV1second"]);
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(planner),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);
        let first_started = starts
            .remove("BV1first")
            .expect("first start signal should exist");
        let mut second_started = starts
            .remove("BV1second")
            .expect("second start signal should exist");
        let first_result = results
            .remove("BV1first")
            .expect("first result sender should exist");
        let second_result = results
            .remove("BV1second")
            .expect("second result sender should exist");

        let first = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1first".to_owned(),
                options: None,
            }))
            .await
            .expect("first playback task should be created")
            .into_inner();
        first_started
            .await
            .expect("first background planner should start");
        let second = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1second".to_owned(),
                options: None,
            }))
            .await
            .expect("second playback task should be created")
            .into_inner();

        tasks
            .cancel_task(&second.id)
            .expect("second task should be cancellable while waiting for permit");
        let cancelled = wait_for_task_state(&tasks, &second.id, TaskState::Cancelled).await;
        assert!(cancelled.playback_session.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut second_started)
                .await
                .is_err(),
            "cancelled task should not start the planner after it was waiting for a permit"
        );

        let recreated = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1second".to_owned(),
                options: None,
            }))
            .await
            .expect("cancelled playback task should allow retry")
            .into_inner();
        assert_ne!(second.id, recreated.id);

        first_result
            .send(Ok(sample_playback_plan()))
            .expect("first plan should be delivered");
        wait_for_task_state(&tasks, &first.id, TaskState::Playable).await;
        second_started
            .await
            .expect("recreated second planner should start after permit is released");
        second_result
            .send(Ok(sample_playback_plan()))
            .expect("second plan should be delivered");
        wait_for_task_state(&tasks, &recreated.id, TaskState::Playable).await;
    }

    #[tokio::test]
    async fn cancelling_playback_task_before_permit_release_does_not_start_planner() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let (planner, mut starts, _results) = SourceControlledPlaybackPlanner::new(["BV1race"]);
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(planner),
        );
        let held_permit = Arc::clone(&state.playback_planning_permits)
            .acquire_owned()
            .await
            .expect("test should acquire the only planning permit");
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);
        let mut started = starts.remove("BV1race").expect("start signal should exist");

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1race".to_owned(),
                options: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut started)
                .await
                .is_err(),
            "planner should wait while the only planning permit is held"
        );

        tasks
            .cancel_task(&created.id)
            .expect("task should be cancellable while waiting for permit");
        drop(held_permit);

        let cancelled = wait_for_task_state(&tasks, &created.id, TaskState::Cancelled).await;
        assert!(cancelled.playback_session.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut started)
                .await
                .is_err(),
            "cancelled task should not start the planner after acquiring a permit"
        );
    }

    struct EmptyPlaybackPlanner;

    impl BilibiliPlaybackPlanner for EmptyPlaybackPlanner {
        fn plan<'a>(
            &'a self,
            _request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            Box::pin(async {
                Ok(BilibiliPlaybackPlan {
                    title: "Empty".to_owned(),
                    entries: Vec::new(),
                })
            })
        }
    }

    struct PendingPlaybackPlanner;

    impl BilibiliPlaybackPlanner for PendingPlaybackPlanner {
        fn plan<'a>(
            &'a self,
            request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            Box::pin(async move {
                while !request.cancellation.is_cancel_requested() {
                    sleep(Duration::from_millis(10)).await;
                }

                Err(BilibiliDownloadError::Cancelled(
                    "Playback planning was cancelled.".to_owned(),
                ))
            })
        }
    }

    struct DeferredPlaybackPlanner {
        started: Mutex<Option<oneshot::Sender<()>>>,
        result:
            Mutex<Option<oneshot::Receiver<Result<BilibiliPlaybackPlan, BilibiliDownloadError>>>>,
    }

    impl DeferredPlaybackPlanner {
        fn new() -> (
            Self,
            oneshot::Receiver<()>,
            oneshot::Sender<Result<BilibiliPlaybackPlan, BilibiliDownloadError>>,
        ) {
            let (started_sender, started_receiver) = oneshot::channel();
            let (result_sender, result_receiver) = oneshot::channel();
            (
                Self {
                    started: Mutex::new(Some(started_sender)),
                    result: Mutex::new(Some(result_receiver)),
                },
                started_receiver,
                result_sender,
            )
        }
    }

    impl BilibiliPlaybackPlanner for DeferredPlaybackPlanner {
        fn plan<'a>(
            &'a self,
            _request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            let started = self
                .started
                .lock()
                .expect("planner signal lock should not be poisoned")
                .take();
            let result = self
                .result
                .lock()
                .expect("planner result lock should not be poisoned")
                .take()
                .expect("planner should be invoked once");
            Box::pin(async move {
                if let Some(sender) = started {
                    let _ = sender.send(());
                }

                result.await.unwrap_or_else(|_| {
                    Err(BilibiliDownloadError::Failed(
                        "Playback planning result channel closed.".to_owned(),
                    ))
                })
            })
        }
    }

    type PlaybackPlanningTestResult = Result<BilibiliPlaybackPlan, BilibiliDownloadError>;
    type PlannerStartReceivers = HashMap<String, oneshot::Receiver<()>>;
    type PlannerResultSenders = HashMap<String, oneshot::Sender<PlaybackPlanningTestResult>>;

    struct SourceControlledPlaybackPlanner {
        starts: Mutex<HashMap<String, oneshot::Sender<()>>>,
        results: Mutex<HashMap<String, oneshot::Receiver<PlaybackPlanningTestResult>>>,
    }

    impl SourceControlledPlaybackPlanner {
        fn new<const N: usize>(
            sources: [&str; N],
        ) -> (Self, PlannerStartReceivers, PlannerResultSenders) {
            let mut starts = HashMap::new();
            let mut start_receivers = HashMap::new();
            let mut results = HashMap::new();
            let mut result_senders = HashMap::new();

            for source in sources {
                let source = source.to_owned();
                let (start_sender, start_receiver) = oneshot::channel();
                let (result_sender, result_receiver) = oneshot::channel();
                starts.insert(source.clone(), start_sender);
                start_receivers.insert(source.clone(), start_receiver);
                results.insert(source.clone(), result_receiver);
                result_senders.insert(source, result_sender);
            }

            (
                Self {
                    starts: Mutex::new(starts),
                    results: Mutex::new(results),
                },
                start_receivers,
                result_senders,
            )
        }
    }

    impl BilibiliPlaybackPlanner for SourceControlledPlaybackPlanner {
        fn plan<'a>(
            &'a self,
            request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            let started = self
                .starts
                .lock()
                .expect("planner start lock should not be poisoned")
                .remove(&request.source)
                .expect("planner source should have a start signal");
            let result = self
                .results
                .lock()
                .expect("planner result lock should not be poisoned")
                .remove(&request.source)
                .expect("planner source should have a result receiver");
            Box::pin(async move {
                let _ = started.send(());
                result.await.unwrap_or_else(|_| {
                    Err(BilibiliDownloadError::Failed(
                        "Playback planning result channel closed.".to_owned(),
                    ))
                })
            })
        }
    }

    async fn wait_for_task_state(
        tasks: &BilibiliTaskRegistry,
        task_id: &str,
        expected_state: TaskState,
    ) -> Task {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let task = tasks
                    .get_task(task_id)
                    .expect("task should exist while waiting for state");
                if task.state() == expected_state {
                    return task;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task should reach expected state")
    }

    fn sample_playback_plan() -> BilibiliPlaybackPlan {
        let selected_variant = playback_variant("h264", "avc1.640028", 1_000_000, 10_000_000);
        BilibiliPlaybackPlan {
            title: "Example".to_owned(),
            entries: vec![BilibiliPlaybackEntry {
                index: 1,
                aid: 1,
                bvid: Some("BV1progressive".to_owned()),
                cid: 1,
                epid: None,
                title: "Episode 1".to_owned(),
                content_id: "BV1progressive-cid1".to_owned(),
                duration_seconds: Some(60),
                abr: BilibiliPlaybackAbrMetadata { groups: Vec::new() },
                selected_variant: Some(BilibiliSelectedPlaybackVariant {
                    variant: selected_variant.clone(),
                    selection: BilibiliPlaybackVariantSelection {
                        policy: BilibiliPlaybackVariantSelectionPolicy::AvPlayerDefault,
                        codec_rank: Some(1),
                        score: 100,
                    },
                }),
                variants: vec![
                    selected_variant,
                    playback_variant("hevc", "hvc1.1.6.L120.90", 2_000_000, 20_000_000),
                ],
            }],
        }
    }

    fn playback_variant(
        id: &str,
        video_codec: &str,
        bandwidth: u64,
        size: u64,
    ) -> AdapterPlaybackVariant {
        AdapterPlaybackVariant {
            id: id.to_owned(),
            kind: BilibiliPlaybackVariantKind::Dash,
            content_id: "BV1progressive-cid1".to_owned(),
            bandwidth: Some(bandwidth),
            codecs: vec![video_codec.to_owned()],
            mime_types: vec!["video/mp4".to_owned()],
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some("60".to_owned()),
            duration_seconds: Some(60),
            abr: None,
            video: Some(media_request(
                BilibiliMediaRequestKind::Video,
                video_codec,
                size,
            )),
            audio: Some(media_request(
                BilibiliMediaRequestKind::Audio,
                "mp4a.40.2",
                1_000_000,
            )),
            flv_segments: Vec::new(),
        }
    }

    fn media_request(
        kind: BilibiliMediaRequestKind,
        codecs: &str,
        size: u64,
    ) -> BilibiliMediaRequest {
        BilibiliMediaRequest {
            kind,
            stream_id: None,
            url: "https://example.test/source.m4s".to_owned(),
            backup_urls: Vec::new(),
            headers: vec![BilibiliHttpHeader {
                name: "referer".to_owned(),
                value: "https://www.bilibili.com".to_owned(),
            }],
            mime_type: Some("video/mp4".to_owned()),
            codecs: Some(codecs.to_owned()),
            bandwidth: None,
            width: None,
            height: None,
            frame_rate: None,
            size: Some(size),
            duration_seconds: Some(60),
            cache_key: BilibiliMediaCacheKey {
                content_id: "BV1progressive-cid1".to_owned(),
                media_kind: kind,
                stream_id: None,
                codecs: Some(codecs.to_owned()),
                source_hash: "source-hash".to_owned(),
            },
        }
    }
}
