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
        HealthState, HealthStatus, LibraryItem, LibrarySource, ListCacheRootsRequest,
        ListCacheRootsResponse, ListLibraryItemsRequest, ListLibraryItemsResponse,
        PlaybackProtocol, PlaybackSource, RescanLibraryRequest, RescanLibraryResponse,
        ServerCapability, ServerInfo, Task, TaskEvent, TaskKind, TaskState, WatchTasksRequest,
        cache_service_server::CacheService, library_service_server::LibraryService,
        server_service_server::ServerService, task_service_server::TaskService,
    },
    hls::HlsPlaybackSession,
    hls_cache::{HlsCacheStore, sanitized_completed_session},
    library::ROOT_ID,
    task_registry::{BilibiliTaskRegistry, current_timestamp},
};

const PLAYBACK_PLANNING_INTERRUPTED_MESSAGE: &str =
    "Playback planning was interrupted before it completed.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsCacheFinalizationFailureMode {
    KeepPlayable,
    FailRestoredTask,
}

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
        let hls_items = filter_hls_library_items(
            self.state.list_completed_hls_library_items(),
            request.filter.as_ref(),
        );
        let hls_count = hls_items.len() as i64;
        let mut items = Vec::with_capacity(page_size as usize);
        let hls_page_start = page_offset < hls_count;
        if page_offset < hls_count {
            items.extend(
                hls_items
                    .into_iter()
                    .skip(page_offset as usize)
                    .take(page_size as usize),
            );
        }
        let remaining = (page_size as usize).saturating_sub(items.len());
        let local_offset = page_offset.saturating_sub(hls_count);
        let mut local_probe_has_items = false;
        let local_page = if remaining > 0 {
            Some(
                self.state
                    .library
                    .list_items_page(request.filter.as_ref(), local_offset, remaining)
                    .await,
            )
        } else if hls_page_start
            && page_offset
                .checked_add(page_size.into())
                .is_some_and(|offset| offset >= hls_count)
        {
            local_probe_has_items = !self
                .state
                .library
                .list_items_page(request.filter.as_ref(), 0, 1)
                .await
                .items
                .is_empty();
            None
        } else {
            None
        };
        if let Some(local_page) = &local_page {
            items.extend(local_page.items.clone());
        }
        let has_more_hls = page_offset + i64::try_from(items.len()).unwrap_or(i64::MAX) < hls_count;
        let next_page_token = if has_more_hls {
            page_offset
                .checked_add(page_size.into())
                .map(|offset| offset.to_string())
                .unwrap_or_default()
        } else {
            local_page
                .and_then(|page| page.next_page_offset)
                .map(|offset| offset + hls_count)
                .map(|offset| offset.to_string())
                .or_else(|| local_probe_has_items.then(|| hls_count.to_string()))
                .unwrap_or_default()
        };

        Ok(Response::new(ListLibraryItemsResponse {
            items,
            next_page_token,
        }))
    }

    async fn get_library_item(
        &self,
        request: Request<GetLibraryItemRequest>,
    ) -> Result<Response<LibraryItem>, Status> {
        let request = request.into_inner();
        if let Some(item) = self.state.get_completed_hls_library_item(&request.id) {
            return Ok(Response::new(item));
        }
        let Some(item) = self.state.library.get_item(&request.id).await else {
            return Err(Status::not_found("Library item not found."));
        };

        Ok(Response::new(item))
    }

    async fn get_playback_source(
        &self,
        request: Request<GetPlaybackSourceRequest>,
    ) -> Result<Response<PlaybackSource>, Status> {
        let item_id = request.get_ref().item_id.clone();
        let variant_id = request.get_ref().variant_id.clone();
        if let Some(session_id) = HlsCacheStore::session_id_from_library_item_id(&item_id) {
            let uri = self
                .state
                .playback_uri_factory
                .create_hls_master_playlist(&request, &session_id);
            if let Some(source) =
                self.state
                    .create_completed_hls_playback_source(&item_id, &variant_id, uri)
            {
                return Ok(Response::new(source));
            }
        }

        if !self.state.library.supports_http_range_playback() {
            return Err(Status::failed_precondition(
                "HTTP range playback is unavailable on this platform.",
            ));
        }

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
            && matches!(task.state(), TaskState::Cancelled | TaskState::Failed)
        {
            self.state.hls_sessions.remove(&task.id);
            let _ = self.state.hls_cache.remove_session(&task.id);
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

fn filter_hls_library_items(
    items: Vec<LibraryItem>,
    filter: Option<&crate::generated::tvos_net_player::v1::LibraryFilter>,
) -> Vec<LibraryItem> {
    let Some(filter) = filter else {
        return items;
    };
    let requested_sources = filter.sources.to_vec();
    if !requested_sources.is_empty()
        && !requested_sources.contains(&(LibrarySource::Bilibili as i32))
    {
        return Vec::new();
    }

    let search_text = filter.search_text.trim().to_lowercase();
    if search_text.is_empty() {
        return items;
    }

    items
        .into_iter()
        .filter(|item| {
            item.title.to_lowercase().contains(&search_text)
                || item.subtitle.to_lowercase().contains(&search_text)
        })
        .collect()
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

    if let Err(error) = state.hls_cache.save_session(&metadata.hls_session) {
        eprintln!(
            "Failed to persist HLS playback manifest for task {task_id}; keeping runtime playback source available: {error}"
        );
    }

    state.hls_sessions.insert(metadata.hls_session.clone());
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
                let _ = state.hls_cache.remove_session(&task_id);
            } else {
                tokio::spawn(run_hls_cache_finalization(
                    state.clone(),
                    task_id.clone(),
                    metadata.hls_session,
                    HlsCacheFinalizationFailureMode::KeepPlayable,
                ));
            }
            cleanup.disarm();
        }
        Err(_) => {
            state.hls_sessions.remove(&task_id);
            let _ = state.hls_cache.remove_session(&task_id);
        }
    }
}

pub(crate) async fn run_hls_cache_finalization(
    state: AppState,
    task_id: String,
    session: HlsPlaybackSession,
    failure_mode: HlsCacheFinalizationFailureMode,
) {
    let permit_request = Arc::clone(&state.hls_cache_finalization_permits).acquire_owned();
    tokio::pin!(permit_request);
    let _permit = loop {
        if !state.tasks.is_playback_task_playable(&task_id) {
            return;
        }
        tokio::select! {
            permit = &mut permit_request => {
                match permit {
                    Ok(permit) => break permit,
                    Err(_) => {
                        eprintln!(
                            "HLS cache finalization limiter is unavailable for task {task_id}."
                        );
                        return;
                    }
                }
            }
            () = sleep(Duration::from_millis(100)) => {}
        }
    };
    let should_cancel = || !state.tasks.is_playback_task_playable(&task_id);
    match state
        .hls_cache
        .cache_session_resources_until(&state.hls_upstream_client, &session, should_cancel)
        .await
    {
        Ok(library_item_id) => {
            let finalized = state
                .tasks
                .complete_playback_cached(&task_id, library_item_id);
            match finalized {
                Ok(task) if task.state() == TaskState::Completed => {
                    state
                        .hls_sessions
                        .insert(sanitized_completed_session(&session));
                }
                Ok(_) | Err(_) => {
                    state.hls_sessions.remove(&task_id);
                    let _ = state.hls_cache.remove_session(&task_id);
                }
            }
        }
        Err(crate::hls_cache::HlsCacheError::Cancelled) => {}
        Err(error) => {
            if !state.tasks.is_playback_task_playable(&task_id) {
                return;
            }
            match failure_mode {
                HlsCacheFinalizationFailureMode::KeepPlayable => {
                    eprintln!(
                        "Failed to finalize HLS playback cache for task {task_id}; keeping runtime playback source available: {error}"
                    );
                }
                HlsCacheFinalizationFailureMode::FailRestoredTask => {
                    state.hls_sessions.remove(&task_id);
                    if let Err(status) = state.tasks.fail_playback_task_after_cache_restore(
                        &task_id,
                        format!("Failed to restore offline HLS cache after restart: {error}"),
                    ) {
                        eprintln!(
                            "Failed to mark restored HLS playback task {task_id} failed after cache finalization error: {status}"
                        );
                    }
                }
            }
        }
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
        fs,
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
            BilibiliPlaybackOptions, CreateBilibiliPlaybackTaskRequest, GetPlaybackSourceRequest,
            LibraryFilter, LibrarySource, ListLibraryItemsRequest, TaskKind, TaskState,
        },
    };
    use axum::{
        Router,
        body::{Body, Bytes},
        http::{HeaderMap, HeaderValue, Response, StatusCode, header::CONTENT_TYPE},
        routing::get,
    };
    use tokio::sync::{mpsc, oneshot};

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
    async fn playback_task_finalizes_cached_hls_library_item_and_restores_after_restart() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let (planner, planner_started, plan_sender) = DeferredPlaybackPlanner::new();
        let state = AppState::new_with_playback_planner(options.clone(), Arc::new(planner));
        let tasks = Arc::clone(&state.tasks);
        let task_service = TaskGrpcService::new(state.clone());
        let library_service = LibraryGrpcService::new(state.clone());

        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1offline".to_owned(),
                options: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();
        planner_started
            .await
            .expect("background playback planner should start");
        plan_sender
            .send(Ok(sample_playback_plan_with_video_url(&upstream_url)))
            .expect("test should send playback plan");

        let completed = wait_for_task_state(&tasks, &created.id, TaskState::Completed).await;
        let expected_item_id = format!("bilibili.hls.{}", completed.id);
        assert_eq!(expected_item_id, completed.library_item_id);
        let completed_source = completed
            .playback_source
            .as_ref()
            .expect("completed task should keep offline playback source");
        assert_eq!(expected_item_id, completed_source.item_id);
        assert_eq!(
            format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                completed.id
            ),
            completed_source.uri
        );

        let library = library_service
            .list_library_items(Request::new(ListLibraryItemsRequest {
                page_token: String::new(),
                page_size: 50,
                filter: Some(LibraryFilter {
                    sources: vec![LibrarySource::Bilibili.into()],
                    search_text: String::new(),
                }),
            }))
            .await
            .expect("library items should list")
            .into_inner();
        assert_eq!(1, library.items.len());
        assert_eq!(expected_item_id, library.items[0].id);

        let playback_source = library_service
            .get_playback_source(Request::new(GetPlaybackSourceRequest {
                item_id: expected_item_id.clone(),
                variant_id: "h264".to_owned(),
            }))
            .await
            .expect("offline HLS item should have a playback source")
            .into_inner();
        assert_eq!(PlaybackProtocol::Hls as i32, playback_source.protocol);
        assert_eq!(
            format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                completed.id
            ),
            playback_source.uri
        );

        let cancel_completed = task_service
            .cancel_task(Request::new(CancelTaskRequest {
                id: completed.id.clone(),
            }))
            .await
            .expect("completed playback task cancel should be idempotent")
            .into_inner();
        assert_eq!(TaskState::Completed, cancel_completed.state());
        assert_eq!(
            expected_item_id,
            cancel_completed
                .playback_source
                .as_ref()
                .expect("completed playback task should keep offline source")
                .item_id
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );

        let restored =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&completed.id)
            .expect("completed playback task should restore");
        assert_eq!(TaskState::Completed, restored_task.state());
        assert_eq!(
            expected_item_id,
            restored_task
                .playback_source
                .as_ref()
                .expect("restored completed task should keep offline source")
                .item_id
        );
        assert!(restored.hls_sessions.get(&completed.id).is_some());
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );

        let hls_session_dir = root_path
            .join(".tvos-net-player")
            .join("hls")
            .join(&completed.id);
        fs::remove_file(hls_session_dir.join("video.m4s.json"))
            .expect("cached resource metadata should be removed");
        let corrupted_restore =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let corrupted_task = corrupted_restore
            .tasks
            .get_task(&completed.id)
            .expect("corrupted completed playback task should remain readable");
        assert_eq!(TaskState::Failed, corrupted_task.state());
        assert!(corrupted_task.library_item_id.is_empty());
        assert!(corrupted_task.playback_source.is_none());
        assert!(corrupted_restore.hls_sessions.get(&completed.id).is_none());
        assert!(
            corrupted_restore
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_none()
        );
        assert!(hls_session_dir.exists());

        let second_corrupted_restore =
            AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let second_corrupted_task = second_corrupted_restore
            .tasks
            .get_task(&completed.id)
            .expect("failed corrupted playback task should remain readable");
        assert_eq!(TaskState::Failed, second_corrupted_task.state());
        assert!(
            second_corrupted_restore
                .hls_sessions
                .get(&completed.id)
                .is_none()
        );
        assert!(hls_session_dir.exists());
    }

    #[tokio::test]
    async fn playback_task_stays_playable_when_hls_manifest_cannot_persist() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        fs::write(root_path.join(".tvos-net-player"), b"not a directory")
            .expect("test should block HLS manifest directory creation");
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
        let task_service = TaskGrpcService::new(state.clone());
        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1runtime".to_owned(),
                options: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();
        planner_started
            .await
            .expect("background playback planner should start");
        plan_sender
            .send(Ok(sample_playback_plan_with_video_url(&upstream_url)))
            .expect("test should send playback plan");

        let playable = wait_for_task_state(&state.tasks, &created.id, TaskState::Playable).await;
        assert!(playable.playback_source.is_some());
        assert!(state.hls_sessions.get(&created.id).is_some());
        tokio::time::sleep(Duration::from_millis(150)).await;
        let still_playable = state
            .tasks
            .get_task(&created.id)
            .expect("playback task should remain readable");
        assert_eq!(TaskState::Playable, still_playable.state());
    }

    #[tokio::test]
    async fn app_state_resumes_incomplete_hls_cache_finalization_after_restart() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1offline", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let playable = state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");
        let expected_item_id = format!("bilibili.hls.{}", playable.id);
        assert_eq!(TaskState::Playable, playable.state());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_none()
        );

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let completed =
            wait_for_task_state(&restored.tasks, &creation.task.id, TaskState::Completed).await;

        assert_eq!(expected_item_id, completed.library_item_id);
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );
    }

    #[tokio::test]
    async fn app_state_fails_restored_hls_task_when_cache_finalization_fails() {
        let (upstream_url, _upstream_task) = start_failing_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1offline", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let playable = state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");
        assert_eq!(TaskState::Playable, playable.state());

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let failed =
            wait_for_task_state(&restored.tasks, &creation.task.id, TaskState::Failed).await;

        assert!(
            failed
                .message
                .contains("Failed to restore offline HLS cache")
        );
        assert!(failed.playback_source.is_none());
        assert!(failed.playback_session.is_none());
        assert!(failed.library_item_id.is_empty());
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&format!("bilibili.hls.{}", creation.task.id))
                .is_none()
        );
        assert!(
            root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&creation.task.id)
                .exists()
        );
    }

    #[tokio::test]
    async fn app_state_completes_playable_hls_task_when_cache_finished_before_restart() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1offline", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let playable = state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");
        let expected_item_id = format!("bilibili.hls.{}", playable.id);

        let cached_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &metadata.hls_session)
            .await
            .expect("HLS resources should cache");
        assert_eq!(expected_item_id, cached_item_id);
        let still_playable = state
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should remain readable");
        assert_eq!(TaskState::Playable, still_playable.state());
        assert!(still_playable.library_item_id.is_empty());

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let completed = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should restore");

        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(expected_item_id, completed.library_item_id);
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );
    }

    #[tokio::test]
    async fn app_state_scrubs_completed_hls_manifest_during_restart_recovery() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1offline", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        let mut hls_session = metadata.hls_session.clone();
        add_sensitive_upstream_request_data(&mut hls_session);
        state
            .hls_cache
            .save_session(&hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");
        let expected_item_id = format!("bilibili.hls.{}", creation.task.id);

        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &hls_session)
            .await
            .expect("HLS resources should cache");
        let manifest_path = root_path
            .join(".tvos-net-player")
            .join("hls")
            .join(&creation.task.id)
            .join("session.json");
        state
            .hls_cache
            .save_session(&hls_session)
            .expect("test should restore the pre-scrub crash-window manifest");
        let pre_restore_manifest =
            fs::read_to_string(&manifest_path).expect("manifest should be readable");
        assert!(pre_restore_manifest.contains("secret-token"));
        assert!(pre_restore_manifest.contains("SESSDATA"));

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let completed = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should restore as completed");
        let post_restore_manifest =
            fs::read_to_string(&manifest_path).expect("manifest should remain readable");
        let restored_session = restored
            .hls_sessions
            .get(&creation.task.id)
            .expect("completed cache should keep a runtime HLS session");

        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(expected_item_id, completed.library_item_id);
        assert!(!post_restore_manifest.contains(&upstream_url));
        assert!(!post_restore_manifest.contains("secret-token"));
        assert!(!post_restore_manifest.contains("SESSDATA"));
        assert!(restored_session.variant.video.request.url.is_empty());
        assert!(
            restored_session
                .variant
                .video
                .request
                .backup_urls
                .is_empty()
        );
        assert!(restored_session.variant.video.request.headers.is_empty());
    }

    #[tokio::test]
    async fn hls_cache_finalizer_sanitizes_runtime_session_after_completion() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1offline", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        let mut hls_session = metadata.hls_session.clone();
        add_sensitive_upstream_request_data(&mut hls_session);
        state
            .hls_cache
            .save_session(&hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");

        run_hls_cache_finalization(
            state.clone(),
            creation.task.id.clone(),
            hls_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        )
        .await;
        let completed = state
            .tasks
            .get_task(&creation.task.id)
            .expect("task should remain readable");
        let runtime_session = state
            .hls_sessions
            .get(&creation.task.id)
            .expect("completed cache should keep a runtime HLS session");

        assert_eq!(TaskState::Completed, completed.state());
        assert!(runtime_session.variant.video.request.url.is_empty());
        assert!(runtime_session.variant.video.request.backup_urls.is_empty());
        assert!(runtime_session.variant.video.request.headers.is_empty());
    }

    #[tokio::test]
    async fn app_state_hides_cancelled_hls_cache_session_after_restart() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1cancelled", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");
        let expected_item_id = format!("bilibili.hls.{}", creation.task.id);
        let cached_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &metadata.hls_session)
            .await
            .expect("HLS resources should cache");
        assert_eq!(expected_item_id, cached_item_id);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );

        let cancelled = state
            .tasks
            .cancel_task(&creation.task.id)
            .expect("playback task should cancel");
        assert_eq!(TaskState::Cancelled, cancelled.state());

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("cancelled playback task should restore");
        assert_eq!(TaskState::Cancelled, restored_task.state());
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );
        let restored_library = LibraryGrpcService::new(restored.clone())
            .list_library_items(Request::new(ListLibraryItemsRequest {
                page_token: String::new(),
                page_size: 50,
                filter: Some(LibraryFilter {
                    sources: vec![LibrarySource::Bilibili.into()],
                    search_text: String::new(),
                }),
            }))
            .await
            .expect("library items should list")
            .into_inner();
        assert!(restored_library.items.is_empty());
        assert!(
            root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&creation.task.id)
                .exists()
        );
    }

    #[tokio::test]
    async fn app_state_hides_hls_cache_when_task_state_snapshot_is_unreadable() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let task_state_path = root_path.join(".state").join("tasks.json");
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: task_state_path.clone(),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1corrupt-state", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");
        let expected_item_id = format!("bilibili.hls.{}", creation.task.id);
        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &metadata.hls_session)
            .await
            .expect("HLS resources should cache");
        state
            .tasks
            .complete_playback_cached(&creation.task.id, expected_item_id.clone())
            .expect("task should complete with cached HLS item");

        fs::write(&task_state_path, b"not json").expect("task snapshot should be corrupted");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        assert!(restored.tasks.get_task(&creation.task.id).is_err());
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());
        assert!(
            root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&creation.task.id)
                .exists()
        );
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );

        let restored_library = LibraryGrpcService::new(restored)
            .list_library_items(Request::new(ListLibraryItemsRequest {
                page_token: String::new(),
                page_size: 50,
                filter: Some(LibraryFilter {
                    sources: vec![LibrarySource::Bilibili.into()],
                    search_text: String::new(),
                }),
            }))
            .await
            .expect("library items should list")
            .into_inner();
        assert!(restored_library.items.is_empty());
    }

    #[tokio::test]
    async fn list_library_items_paginates_from_hls_cache_to_local_library() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        fs::write(root_path.join("local.mp4"), fake_mp4())
            .expect("local media file should be written");
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let (planner, planner_started, plan_sender) = DeferredPlaybackPlanner::new();
        let state = AppState::new_with_playback_planner(options, Arc::new(planner));
        let tasks = Arc::clone(&state.tasks);
        let task_service = TaskGrpcService::new(state.clone());
        let library_service = LibraryGrpcService::new(state);

        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1offline".to_owned(),
                options: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();
        planner_started
            .await
            .expect("background playback planner should start");
        plan_sender
            .send(Ok(sample_playback_plan_with_video_url(&upstream_url)))
            .expect("test should send playback plan");
        let completed = wait_for_task_state(&tasks, &created.id, TaskState::Completed).await;
        let expected_item_id = format!("bilibili.hls.{}", completed.id);

        let first_page = library_service
            .list_library_items(Request::new(ListLibraryItemsRequest {
                page_token: String::new(),
                page_size: 1,
                filter: None,
            }))
            .await
            .expect("first page should list")
            .into_inner();

        assert_eq!(vec![expected_item_id], item_ids(&first_page.items));
        assert_eq!("1", first_page.next_page_token);

        let second_page = library_service
            .list_library_items(Request::new(ListLibraryItemsRequest {
                page_token: first_page.next_page_token,
                page_size: 1,
                filter: None,
            }))
            .await
            .expect("second page should list")
            .into_inner();

        assert_eq!(1, second_page.items.len());
        assert!(second_page.items[0].id.starts_with("local.default."));
    }

    #[tokio::test]
    async fn hls_cache_finalizer_removes_cache_when_task_was_cancelled() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1offline", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let playable = state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");
        assert_eq!(TaskState::Playable, playable.state());

        let task_service = TaskGrpcService::new(state.clone());
        let cancelled = task_service
            .cancel_task(Request::new(CancelTaskRequest {
                id: creation.task.id.clone(),
            }))
            .await
            .expect("playable task should cancel")
            .into_inner();
        assert_eq!(TaskState::Cancelled, cancelled.state());
        assert!(state.hls_sessions.get(&creation.task.id).is_none());

        run_hls_cache_finalization(
            state.clone(),
            creation.task.id.clone(),
            metadata.hls_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        )
        .await;

        let final_task = state
            .tasks
            .get_task(&creation.task.id)
            .expect("cancelled task should still exist");
        assert_eq!(TaskState::Cancelled, final_task.state());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&format!("bilibili.hls.{}", creation.task.id))
                .is_none()
        );
        assert!(state.hls_sessions.get(&creation.task.id).is_none());
    }

    #[tokio::test]
    async fn hls_cache_finalizer_stops_when_task_is_cancelled_during_download() {
        let (upstream_url, _upstream_task, first_chunk_received) =
            start_blocked_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1offline", None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let playable = state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");
        assert_eq!(TaskState::Playable, playable.state());

        let finalizer = tokio::spawn(run_hls_cache_finalization(
            state.clone(),
            creation.task.id.clone(),
            metadata.hls_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        first_chunk_received
            .await
            .expect("upstream should send the first chunk");

        let task_service = TaskGrpcService::new(state.clone());
        let cancelled = task_service
            .cancel_task(Request::new(CancelTaskRequest {
                id: creation.task.id.clone(),
            }))
            .await
            .expect("playable task should cancel")
            .into_inner();
        assert_eq!(TaskState::Cancelled, cancelled.state());

        tokio::time::timeout(Duration::from_secs(2), finalizer)
            .await
            .expect("finalizer should stop after cancellation")
            .expect("finalizer task should not panic");
        let final_task = state
            .tasks
            .get_task(&creation.task.id)
            .expect("cancelled task should still exist");
        assert_eq!(TaskState::Cancelled, final_task.state());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&format!("bilibili.hls.{}", creation.task.id))
                .is_none()
        );
        assert!(state.hls_sessions.get(&creation.task.id).is_none());
        assert!(
            !root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&creation.task.id)
                .join("video.m4s.tmp")
                .exists()
        );
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

    fn sample_playback_plan_with_video_url(url: &str) -> BilibiliPlaybackPlan {
        let selected_variant = playback_variant_with_url("h264", "avc1.640028", 1_000_000, url);
        BilibiliPlaybackPlan {
            title: "Example".to_owned(),
            entries: vec![BilibiliPlaybackEntry {
                index: 1,
                aid: 1,
                bvid: Some("BV1offline".to_owned()),
                cid: 1,
                epid: None,
                title: "Offline Episode".to_owned(),
                content_id: "BV1offline-cid1".to_owned(),
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
                variants: vec![selected_variant],
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

    fn playback_variant_with_url(
        id: &str,
        video_codec: &str,
        bandwidth: u64,
        url: &str,
    ) -> AdapterPlaybackVariant {
        AdapterPlaybackVariant {
            id: id.to_owned(),
            kind: BilibiliPlaybackVariantKind::Dash,
            content_id: "BV1offline-cid1".to_owned(),
            bandwidth: Some(bandwidth),
            codecs: vec![video_codec.to_owned()],
            mime_types: vec!["video/mp4".to_owned()],
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some("60".to_owned()),
            duration_seconds: Some(60),
            abr: None,
            video: Some(media_request_with_url(
                BilibiliMediaRequestKind::Video,
                video_codec,
                url,
            )),
            audio: None,
            flv_segments: Vec::new(),
        }
    }

    fn media_request(
        kind: BilibiliMediaRequestKind,
        codecs: &str,
        size: u64,
    ) -> BilibiliMediaRequest {
        media_request_with_size(kind, codecs, "https://example.test/source.m4s", Some(size))
    }

    fn media_request_with_url(
        kind: BilibiliMediaRequestKind,
        codecs: &str,
        url: &str,
    ) -> BilibiliMediaRequest {
        media_request_with_size(kind, codecs, url, None)
    }

    fn media_request_with_size(
        kind: BilibiliMediaRequestKind,
        codecs: &str,
        url: &str,
        size: Option<u64>,
    ) -> BilibiliMediaRequest {
        BilibiliMediaRequest {
            kind,
            stream_id: None,
            url: url.to_owned(),
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
            size,
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

    fn add_sensitive_upstream_request_data(session: &mut HlsPlaybackSession) {
        session.variant.video.request.backup_urls =
            vec!["https://cdn-backup.example.test/video.m4s".to_owned()];
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
    }

    fn item_ids(items: &[LibraryItem]) -> Vec<String> {
        items.iter().map(|item| item.id.clone()).collect()
    }

    async fn start_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/video.m4s", get(upstream_mp4)),
            )
            .await
            .expect("upstream should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_failing_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/video.m4s", get(upstream_unavailable)),
            )
            .await
            .expect("upstream should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_blocked_mp4_upstream()
    -> (String, tokio::task::JoinHandle<()>, oneshot::Receiver<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let (first_chunk_sender, first_chunk_receiver) = oneshot::channel();
        let first_chunk_sender = Arc::new(Mutex::new(Some(first_chunk_sender)));
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get({
                        let first_chunk_sender = Arc::clone(&first_chunk_sender);
                        move |headers| {
                            blocked_upstream_mp4(headers, Arc::clone(&first_chunk_sender))
                        }
                    }),
                ),
            )
            .await
            .expect("upstream should run");
        });

        (
            format!("http://{addr}/video.m4s"),
            task,
            first_chunk_receiver,
        )
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

    async fn upstream_unavailable(_headers: HeaderMap) -> Response<Body> {
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::empty())
            .unwrap()
    }

    async fn blocked_upstream_mp4(
        headers: HeaderMap,
        first_chunk_sender: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    ) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        tokio::spawn(async move {
            let bytes = fake_mp4();
            let split_at = bytes.len().min(16);
            if sender
                .send(Ok(Bytes::copy_from_slice(&bytes[..split_at])))
                .await
                .is_ok()
            {
                if let Some(first_chunk_sender) = first_chunk_sender
                    .lock()
                    .expect("signal lock poisoned")
                    .take()
                {
                    let _ = first_chunk_sender.send(());
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                let _ = sender
                    .send(Ok(Bytes::copy_from_slice(&bytes[split_at..])))
                    .await;
            }
        });

        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .body(Body::from_stream(ReceiverStream::new(receiver)))
            .unwrap()
    }

    fn fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes.extend(mp4_box(*b"moof", b"frag"));
        bytes.extend(mp4_box(*b"mdat", b"media-data"));
        bytes
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
