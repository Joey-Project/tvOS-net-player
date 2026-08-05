use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime},
};

use bbdown_core::{
    CredentialProfileSelection, CredentialProfiles, CredentialStore, Credentials,
    DEFAULT_CREDENTIAL_PROFILE,
};
use futures_core::Stream;
use tokio::{sync::mpsc, time::sleep};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::{
    AppState,
    bbdown_adapter::{
        BilibiliPlaybackPlan, BilibiliPlaybackVariant as AdapterPlaybackVariant,
        BilibiliPlaybackVariantKind, recover_stable_collection_candidate,
    },
    bilibili_playback::{
        BilibiliInputResolution, BilibiliInputResolveRequest, BilibiliPlaybackPlanningRequest,
        BilibiliResolvedCandidate as AdapterBilibiliResolvedCandidate,
    },
    bilibili_worker::BilibiliDownloadError,
    config::{BbdownRestrictedArea, CacheServerOptions},
    generated::tvos_net_player::v1::{
        BilibiliCredentialProfile, BilibiliCredentialState, BilibiliCredentialStatus,
        BilibiliLoginMethod, BilibiliLoginSession, BilibiliLoginSessionState,
        BilibiliPlaybackOptions, BilibiliPlaybackSession, BilibiliPlaybackVariant,
        BilibiliResolveResult, BilibiliResolvedCandidate as ProtoBilibiliResolvedCandidate,
        BilibiliTaskResultItem, BilibiliTaskSelection, CacheRoot, CancelTaskRequest,
        CheckHealthRequest, CreateBilibiliPlaybackTaskRequest, CreateBilibiliTaskRequest,
        DeleteLibraryItemRequest, DeleteLibraryItemResponse, GetBilibiliCredentialStatusRequest,
        GetBilibiliLoginSessionRequest, GetHlsCacheStatusRequest, GetLibraryItemRequest,
        GetPlaybackSourceRequest, GetServerInfoRequest, GetTaskRequest, HealthState, HealthStatus,
        HlsCacheEvictionSummary as ProtoHlsCacheEvictionSummary, HlsCacheStatus,
        HlsPlaybackActivityState as ProtoHlsPlaybackActivityState, HlsPlaybackProgressStatus,
        HlsWeakNetworkState, HlsWeakNetworkStatus, LanTranscodingPlan, LanTranscodingPlanState,
        LanTranscodingRuntimeState as ProtoLanTranscodingRuntimeState, LanTranscodingStatus,
        LibraryItem, LibrarySource, ListBilibiliCredentialProfilesRequest,
        ListBilibiliCredentialProfilesResponse, ListCacheRootsRequest, ListCacheRootsResponse,
        ListLibraryItemsRequest, ListLibraryItemsResponse,
        PlaybackProgressIntent as ProtoPlaybackProgressIntent, PlaybackProtocol, PlaybackSource,
        ReportPlaybackProgressRequest, ReportPlaybackProgressResponse, RescanLibraryRequest,
        RescanLibraryResponse, ResolveBilibiliInputRequest, ServerCapability, ServerInfo,
        StartBilibiliLoginSessionRequest, Task, TaskEvent, TaskKind, TaskState, WatchTasksRequest,
        cache_service_server::CacheService, library_service_server::LibraryService,
        server_service_server::ServerService, task_service_server::TaskService,
    },
    hls::{HlsPlaybackSession, HlsVariant, HlsVariantMetadata},
    hls_cache::{
        HlsCacheEvictionSummary, HlsCacheFillControl, HlsCacheFillProgress, HlsCacheStore,
        hls_session_declared_size_bytes, timestamp_from_system_time,
    },
    hls_fill_scheduler::HlsFillPreemptionToken,
    hls_network_policy::{
        HlsWeakNetworkSnapshot, HlsWeakNetworkState as RuntimeHlsWeakNetworkState,
    },
    hls_playback_progress::{
        HlsPlaybackActivityState, HlsPlaybackProgressSnapshot, PlaybackProgressIntent,
        PlaybackProgressReport,
    },
    library::ROOT_ID,
    playback_policy::PlaybackPolicy,
    task_registry::{
        BilibiliTaskProgress, BilibiliTaskRegistry, PLAYBACK_PLANNING_CANCELLED_MESSAGE,
        PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE, current_timestamp,
    },
    transcoding::{
        HlsTranscodingPlan, HlsTranscodingPlanState, LanTranscodingRuntimeState,
        LanTranscodingStatusSnapshot,
    },
};

const PLAYBACK_PLANNING_INTERRUPTED_MESSAGE: &str =
    "Playback planning was interrupted before it completed.";
const HLS_CACHE_PROGRESS_PUBLISH_MIN_BYTES: u64 = 1024 * 1024;
const BILIBILI_TASK_SELECTION_MODE_UNSPECIFIED: i32 = 0;
const BILIBILI_TASK_SELECTION_MODE_DEFAULT: i32 = 1;
const BILIBILI_TASK_SELECTION_MODE_CURRENT: i32 = 2;
const BILIBILI_TASK_SELECTION_MODE_SINGLE: i32 = 3;
const BILIBILI_TASK_SELECTION_MODE_MULTIPLE: i32 = 4;
const BILIBILI_TASK_SELECTION_MODE_RANGE: i32 = 5;
const BILIBILI_TASK_SELECTION_MODE_ALL: i32 = 6;
const BILIBILI_RESULT_PLANNING_MESSAGE: &str = "Queued for Bilibili playback planning.";
const BILIBILI_RESULT_PLAYABLE_MESSAGE: &str = "Playable online.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsCacheFinalizationFailureMode {
    KeepPlayable,
    FailRestoredTask,
}

#[derive(Clone)]
pub struct ServerGrpcService {
    state: AppState,
    login_sessions: Arc<StdMutex<VecDeque<BilibiliLoginSession>>>,
}

const MAX_BILIBILI_LOGIN_SESSIONS: usize = 64;
const MAX_BILIBILI_LOGIN_PROFILE_ID_BYTES: usize = 256;

impl ServerGrpcService {
    pub fn new(state: AppState) -> Self {
        let login_sessions = Arc::clone(&state.bilibili_login_sessions);
        Self {
            state,
            login_sessions,
        }
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
                ServerCapability::BilibiliResolve.into(),
                ServerCapability::BilibiliTaskSelection.into(),
                ServerCapability::BilibiliCredentialStatus.into(),
                ServerCapability::BilibiliCredentialProfiles.into(),
                ServerCapability::BilibiliPlaybackPolicy.into(),
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
        if self.state.options.allow_library_item_delete {
            info.capabilities
                .push(ServerCapability::LibraryItemDelete.into());
        }
        if self.state.options.lan_transcoding_enabled {
            info.capabilities
                .push(ServerCapability::LanTranscoding.into());
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

    async fn get_bilibili_credential_status(
        &self,
        _request: Request<GetBilibiliCredentialStatusRequest>,
    ) -> Result<Response<BilibiliCredentialStatus>, Status> {
        Ok(Response::new(bilibili_credential_status(
            &self.state.options,
        )))
    }

    async fn list_bilibili_credential_profiles(
        &self,
        _request: Request<ListBilibiliCredentialProfilesRequest>,
    ) -> Result<Response<ListBilibiliCredentialProfilesResponse>, Status> {
        Ok(Response::new(bilibili_credential_profiles(
            &self.state.options,
        )?))
    }

    async fn start_bilibili_login_session(
        &self,
        request: Request<StartBilibiliLoginSessionRequest>,
    ) -> Result<Response<BilibiliLoginSession>, Status> {
        let request = request.into_inner();
        let method = match BilibiliLoginMethod::try_from(request.method)
            .map_err(|_| Status::invalid_argument("Unsupported Bilibili login method."))?
        {
            BilibiliLoginMethod::Unspecified => BilibiliLoginMethod::WebQr,
            method => method,
        };
        let profile_id = normalize_login_profile_id(&request.profile_id, &self.state.options)?;
        let session = BilibiliLoginSession {
            id: uuid::Uuid::new_v4().to_string(),
            profile_id,
            method: method.into(),
            state: BilibiliLoginSessionState::Unsupported.into(),
            message: "Bilibili login session control-plane is available, but server-side QR login is not implemented in this slice.".to_owned(),
            verification_uri: String::new(),
            created_at: Some(current_timestamp()),
            expires_at: None,
        };
        let mut login_sessions = self
            .login_sessions
            .lock()
            .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?;
        if login_sessions.len() >= MAX_BILIBILI_LOGIN_SESSIONS {
            login_sessions.pop_front();
        }
        login_sessions.push_back(session.clone());
        Ok(Response::new(session))
    }

    async fn get_bilibili_login_session(
        &self,
        request: Request<GetBilibiliLoginSessionRequest>,
    ) -> Result<Response<BilibiliLoginSession>, Status> {
        let session_id = request.into_inner().session_id;
        let session = self
            .login_sessions
            .lock()
            .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
            .ok_or_else(|| Status::not_found("Bilibili login session not found."))?;
        Ok(Response::new(session))
    }
}

fn bilibili_credential_status(
    options: &crate::config::CacheServerOptions,
) -> BilibiliCredentialStatus {
    let restricted_area_configured = options.bbdown_restricted_area.is_some()
        || !options.bbdown_restricted_area_proxies.is_empty()
        || !options.bbdown_restricted_api_proxies.is_empty();
    let restricted_area = options
        .bbdown_restricted_area
        .map(restricted_area_label)
        .unwrap_or_default()
        .to_owned();
    let base_status = || BilibiliCredentialStatus {
        state: BilibiliCredentialState::Unspecified.into(),
        message: String::new(),
        credential_path_configured: options.bbdown_credential_path.is_some(),
        credential_file_loaded: false,
        web_cookie_present: false,
        access_key_present: false,
        tv_access_key_present: false,
        restricted_area: restricted_area.clone(),
        restricted_playurl_proxy_count: options.bbdown_restricted_area_proxies.len() as u32,
        restricted_api_proxy_count: options.bbdown_restricted_api_proxies.len() as u32,
        checked_at: Some(current_timestamp()),
        active_profile_id: options
            .bbdown_credential_profile
            .clone()
            .unwrap_or_default(),
        default_profile_id: String::new(),
        profile_count: 0,
        profiles: Vec::new(),
    };

    let Some(path) = options.bbdown_credential_path.as_ref() else {
        let mut status = base_status();
        if restricted_area_configured {
            status.state = BilibiliCredentialState::Degraded.into();
            status.message =
                "Restricted-area settings are configured without a BBDown credential file."
                    .to_owned();
        } else {
            status.state = BilibiliCredentialState::NotConfigured.into();
            status.message = "No BBDown credential file is configured.".to_owned();
        }
        return status;
    };

    if !path.is_file() {
        let mut status = base_status();
        status.state = BilibiliCredentialState::Error.into();
        status.message = "Failed to load BBDown credential file.".to_owned();
        return status;
    }

    match CredentialStore::new(path.clone()).load_profiles() {
        Ok(profiles) => {
            let active_profile_id = options
                .bbdown_credential_profile
                .clone()
                .unwrap_or_else(|| profiles.default_profile.clone());
            if options.bbdown_credential_profile.is_some()
                && !profiles
                    .profile_names()
                    .any(|name| name == active_profile_id)
            {
                let mut status = base_status();
                status.credential_file_loaded = true;
                status.default_profile_id = profiles.default_profile;
                status.active_profile_id = active_profile_id;
                status.state = BilibiliCredentialState::Error.into();
                status.message = "Configured BBDown credential profile was not found.".to_owned();
                return status;
            }
            let credentials = profiles
                .profile(&active_profile_id)
                .unwrap_or_else(|_| Credentials::default());
            let profile_summaries = credential_profile_summaries(&profiles, &active_profile_id);
            let mut status = base_status();
            status.credential_file_loaded = true;
            status.default_profile_id = profiles.default_profile;
            status.active_profile_id = active_profile_id;
            status.profile_count = profile_summaries.len() as u32;
            status.profiles = profile_summaries;
            status.web_cookie_present = credentials
                .cookie
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty());
            status.access_key_present = credentials
                .access_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty());
            status.tv_access_key_present = credentials
                .tv_access_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty());
            if status.web_cookie_present
                || status.access_key_present
                || status.tv_access_key_present
            {
                status.state = BilibiliCredentialState::Ready.into();
                status.message = "BBDown credential file loaded.".to_owned();
            } else {
                status.state = BilibiliCredentialState::Degraded.into();
                status.message =
                    "BBDown credential file loaded but contains no credential material.".to_owned();
            }
            status
        }
        Err(_) => {
            let mut status = base_status();
            status.state = BilibiliCredentialState::Error.into();
            status.message = "Failed to load BBDown credential file.".to_owned();
            status
        }
    }
}

fn bilibili_credential_profiles(
    options: &crate::config::CacheServerOptions,
) -> Result<ListBilibiliCredentialProfilesResponse, Status> {
    let Some(path) = options.bbdown_credential_path.as_ref() else {
        return Ok(ListBilibiliCredentialProfilesResponse {
            profiles: Vec::new(),
            active_profile_id: options
                .bbdown_credential_profile
                .clone()
                .unwrap_or_default(),
            default_profile_id: String::new(),
            checked_at: Some(current_timestamp()),
        });
    };
    if !path.is_file() {
        return Err(Status::failed_precondition(
            "Failed to load BBDown credential file.",
        ));
    }
    let profiles = CredentialStore::new(path.clone())
        .load_profiles()
        .map_err(|_| Status::failed_precondition("Failed to load BBDown credential file."))?;
    let active_profile_id = options
        .bbdown_credential_profile
        .clone()
        .unwrap_or_else(|| profiles.default_profile.clone());
    if options.bbdown_credential_profile.is_some()
        && !profiles
            .profile_names()
            .any(|name| name == active_profile_id)
    {
        return Err(Status::failed_precondition(
            "Configured BBDown credential profile was not found.",
        ));
    }
    let profile_summaries = credential_profile_summaries(&profiles, &active_profile_id);
    Ok(ListBilibiliCredentialProfilesResponse {
        profiles: profile_summaries,
        active_profile_id,
        default_profile_id: profiles.default_profile,
        checked_at: Some(current_timestamp()),
    })
}

fn credential_profile_summaries(
    profiles: &CredentialProfiles,
    active_profile_id: &str,
) -> Vec<BilibiliCredentialProfile> {
    let mut names = BTreeSet::new();
    names.insert(profiles.default_profile.clone());
    names.extend(profiles.profile_names().map(ToOwned::to_owned));
    names
        .into_iter()
        .map(|name| {
            let credentials = profiles
                .profile(&name)
                .unwrap_or_else(|_| Credentials::default());
            BilibiliCredentialProfile {
                id: name.clone(),
                is_default: name == profiles.default_profile,
                is_active: name == active_profile_id,
                web_cookie_present: credentials
                    .cookie
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty()),
                access_key_present: credentials
                    .access_key
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty()),
                tv_access_key_present: credentials
                    .tv_access_key
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty()),
            }
        })
        .collect()
}

fn normalize_login_profile_id(
    requested_profile_id: &str,
    options: &CacheServerOptions,
) -> Result<String, Status> {
    let requested_profile_id = requested_profile_id.trim();
    let profile_id = if requested_profile_id.is_empty() {
        if let Some(profile) = options.bbdown_credential_profile.as_ref() {
            profile.clone()
        } else if let Some(path) = options.bbdown_credential_path.as_ref() {
            CredentialStore::new(path.clone())
                .load_profiles()
                .map(|profiles| profiles.default_profile)
                .map_err(|_| {
                    Status::failed_precondition("Failed to load BBDown credential file.")
                })?
        } else {
            DEFAULT_CREDENTIAL_PROFILE.to_owned()
        }
    } else {
        let selection =
            CredentialProfileSelection::named(requested_profile_id).map_err(|error| {
                Status::invalid_argument(format!("Invalid Bilibili profile ID: {error}"))
            })?;
        selection
            .profile_name()
            .map(ToOwned::to_owned)
            .ok_or_else(|| Status::invalid_argument("Invalid Bilibili profile ID."))?
    };
    if profile_id.len() > MAX_BILIBILI_LOGIN_PROFILE_ID_BYTES {
        return Err(Status::invalid_argument(format!(
            "Bilibili profile ID must not exceed {MAX_BILIBILI_LOGIN_PROFILE_ID_BYTES} UTF-8 bytes."
        )));
    }
    Ok(profile_id)
}

fn restricted_area_label(area: BbdownRestrictedArea) -> &'static str {
    match area {
        BbdownRestrictedArea::Cn => "cn",
        BbdownRestrictedArea::Th => "th",
        BbdownRestrictedArea::Hk => "hk",
        BbdownRestrictedArea::Tw => "tw",
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

    async fn resolve_bilibili_input(
        &self,
        request: Request<ResolveBilibiliInputRequest>,
    ) -> Result<Response<BilibiliResolveResult>, Status> {
        let request = request.into_inner();
        let source = request.url_or_id.trim().to_owned();
        if source.is_empty() {
            return Err(Status::invalid_argument("Bilibili URL or id is required."));
        }

        let _permit = Arc::clone(&self.state.playback_planning_permits)
            .acquire_owned()
            .await
            .map_err(|_| {
                Status::unavailable("Playback planning concurrency limiter is unavailable.")
            })?;
        let resolution = self
            .state
            .playback_planner
            .resolve_input(crate::bilibili_playback::BilibiliInputResolveRequest {
                source,
                options: request.options,
                cancellation: crate::task_registry::BilibiliTaskCancellation::default(),
            })
            .await
            .map_err(|error| playback_status_from_error(&self.state, error))?;
        Ok(Response::new(BilibiliResolveResult::from(resolution)))
    }

    async fn create_bilibili_task(
        &self,
        request: Request<CreateBilibiliTaskRequest>,
    ) -> Result<Response<Task>, Status> {
        let request = request.into_inner();
        let task = self
            .state
            .tasks
            .create_bilibili_task(&request.url_or_id, request.options)?;
        Ok(Response::new(task_for_client(
            task,
            self.state.bilibili_error_details_are_sensitive(),
        )))
    }

    async fn create_bilibili_playback_task(
        &self,
        request: Request<CreateBilibiliPlaybackTaskRequest>,
    ) -> Result<Response<Task>, Status> {
        let url_or_id = request.get_ref().url_or_id.clone();
        let options = request.get_ref().options.clone();
        let selection_plan = playback_selection_plan(
            normalized_optional_string(&request.get_ref().selection_id),
            request.get_ref().selection.clone(),
        )?;
        let creation = self.state.tasks.create_bilibili_playback_task(
            &url_or_id,
            options.clone(),
            selection_plan.task_selection.clone(),
        )?;
        if !creation.created {
            return Ok(Response::new(task_for_client(
                creation.task,
                self.state.bilibili_error_details_are_sensitive(),
            )));
        }

        let task_id = creation.task.id.clone();
        let playback_source_uri = self
            .state
            .playback_uri_factory
            .create_hls_master_playlist(&request, &task_id);
        let cancellation = creation
            .cancellation
            .expect("new playback task should include a planning cancellation token");
        let task_source = creation.task.source.clone();
        let state = self.state.clone();
        let planning_activity = state.begin_playback_planning();
        tokio::spawn(async move {
            let _planning_activity = planning_activity;
            run_bilibili_playback_planning(
                state,
                task_id,
                task_source,
                options,
                selection_plan,
                playback_source_uri,
                cancellation,
            )
            .await;
        });

        Ok(Response::new(task_for_client(
            creation.task,
            self.state.bilibili_error_details_are_sensitive(),
        )))
    }

    async fn get_task(&self, request: Request<GetTaskRequest>) -> Result<Response<Task>, Status> {
        let request = request.into_inner();
        Ok(Response::new(task_for_client(
            self.state.tasks.get_task(&request.id)?,
            self.state.bilibili_error_details_are_sensitive(),
        )))
    }

    async fn watch_tasks(
        &self,
        request: Request<WatchTasksRequest>,
    ) -> Result<Response<Self::WatchTasksStream>, Status> {
        let request = request.into_inner();
        let mut subscription = self.state.tasks.subscribe(&request.ids)?;
        let snapshots = subscription.snapshots().to_vec();
        let redact_error_details = self.state.bilibili_error_details_are_sensitive();
        let (sender, receiver) = mpsc::channel(128);
        tokio::spawn(async move {
            for task in snapshots {
                if sender
                    .send(Ok(TaskEvent {
                        task: Some(task_for_client(task, redact_error_details)),
                    }))
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
                            .send(Ok(TaskEvent {
                                task: Some(task_for_client(task, redact_error_details)),
                            }))
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
        let hls_session_ids = self.state.tasks.playback_hls_session_ids(&request.id);
        let task = self.state.tasks.cancel_task(&request.id)?;
        if task.kind() == TaskKind::BilibiliProgressivePlayback
            && matches!(task.state(), TaskState::Cancelled | TaskState::Failed)
        {
            for session_id in hls_session_ids {
                self.state.remove_hls_playback_session(&session_id);
                let _ = self.state.hls_cache.remove_session(&session_id);
            }
        }
        Ok(Response::new(task_for_client(
            task,
            self.state.bilibili_error_details_are_sensitive(),
        )))
    }
}

fn task_for_client(mut task: Task, redact_error_details: bool) -> Task {
    if !redact_error_details {
        return task;
    }
    match task.state() {
        TaskState::Running | TaskState::CancelRequested
            if task.kind() == TaskKind::BilibiliDownload =>
        {
            task.message = crate::CREDENTIAL_SAFE_CLIENT_RUNNING_DETAIL.to_owned();
        }
        TaskState::Failed => {
            task.message = crate::credential_safe_client_error(true, &task.message);
        }
        TaskState::Cancelled => {
            task.message = crate::credential_safe_client_cancellation(true, &task.message);
        }
        TaskState::Playable | TaskState::Completed
            if task.kind() == TaskKind::BilibiliProgressivePlayback
                && task.message.contains("offline cache fill failed") =>
        {
            task.message = crate::credential_safe_client_error(true, &task.message);
        }
        _ => {}
    }
    for item in &mut task.result_items {
        match item.state() {
            TaskState::Failed => {
                item.message = crate::credential_safe_client_error(true, &item.message);
            }
            TaskState::Cancelled => {
                item.message = crate::credential_safe_client_cancellation(true, &item.message);
            }
            _ => {}
        }
    }
    task
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

    async fn get_hls_cache_status(
        &self,
        _request: Request<GetHlsCacheStatusRequest>,
    ) -> Result<Response<HlsCacheStatus>, Status> {
        let status = self.state.hls_cache_status().map_err(|error| {
            Status::internal(format!("Failed to scan HLS cache status: {error}"))
        })?;
        let weak_network = self.state.hls_weak_network_status();
        let playback = self.state.hls_playback_progress_status();
        Ok(Response::new(HlsCacheStatus {
            eviction_enabled: status.policy.eviction_enabled(),
            max_bytes: i64_from_u64(status.policy.max_bytes),
            high_watermark_percent: i32::from(status.policy.high_watermark_percent),
            low_watermark_percent: i32::from(status.policy.low_watermark_percent),
            high_watermark_bytes: i64_from_u64(status.policy.high_watermark_bytes()),
            low_watermark_bytes: i64_from_u64(status.policy.low_watermark_bytes()),
            used_bytes: i64_from_u64(status.usage.used_bytes),
            completed_session_count: status
                .usage
                .completed_session_count
                .try_into()
                .unwrap_or(i32::MAX),
            last_eviction: status
                .last_eviction
                .as_ref()
                .map(proto_hls_eviction_summary),
            weak_network: Some(proto_hls_weak_network_status(&weak_network)),
            transcoding: Some(proto_lan_transcoding_status(
                &LanTranscodingStatusSnapshot::from_options(
                    &self.state.options,
                    self.state.lan_transcoding_active_job_count(),
                ),
            )),
            playback: Some(proto_hls_playback_progress_status(&playback)),
        }))
    }

    async fn report_playback_progress(
        &self,
        request: Request<ReportPlaybackProgressRequest>,
    ) -> Result<Response<ReportPlaybackProgressResponse>, Status> {
        let request = request.into_inner();
        if !request.position_seconds.is_finite() || request.position_seconds < 0.0 {
            return Err(Status::invalid_argument(
                "Playback position must be a finite non-negative value.",
            ));
        }
        if !request.duration_seconds.is_finite() || request.duration_seconds < 0.0 {
            return Err(Status::invalid_argument(
                "Playback duration must be zero or a finite non-negative value.",
            ));
        }

        let intent = playback_progress_intent_from_proto(request.intent())?;
        let report = PlaybackProgressReport {
            playback_uri: request.playback_uri,
            library_item_id: request.library_item_id,
            variant_id: request.variant_id,
            position_seconds: request.position_seconds,
            duration_seconds: (request.duration_seconds > 0.0).then_some(request.duration_seconds),
            intent,
            reported_at: SystemTime::now(),
        };
        let outcome = self.state.record_hls_playback_progress(report);

        Ok(Response::new(ReportPlaybackProgressResponse {
            accepted: outcome.accepted,
            session_id: outcome.session_id,
            message: outcome.message,
        }))
    }

    async fn delete_library_item(
        &self,
        request: Request<DeleteLibraryItemRequest>,
    ) -> Result<Response<DeleteLibraryItemResponse>, Status> {
        let request = request.into_inner();
        let id = request.id.trim();
        if id.is_empty() {
            return Err(Status::invalid_argument("Library item id is required."));
        }
        if !self.state.options.allow_library_item_delete {
            return Err(Status::permission_denied(
                "Library item deletion is not enabled on this cache server.",
            ));
        }

        if let Some(deleted) = self.state.delete_completed_hls_library_item(id)? {
            return Ok(Response::new(DeleteLibraryItemResponse { deleted }));
        }

        let deleted =
            self.state.library.delete_item(id).await.map_err(|error| {
                Status::internal(format!("Failed to delete library item: {error}"))
            })?;
        Ok(Response::new(DeleteLibraryItemResponse { deleted }))
    }
}

fn proto_hls_eviction_summary(summary: &HlsCacheEvictionSummary) -> ProtoHlsCacheEvictionSummary {
    ProtoHlsCacheEvictionSummary {
        reason: summary.reason.clone(),
        started_used_bytes: i64_from_u64(summary.started_used_bytes),
        finished_used_bytes: i64_from_u64(summary.finished_used_bytes),
        target_used_bytes: i64_from_u64(summary.target_used_bytes),
        projected_added_bytes: i64_from_u64(summary.projected_added_bytes),
        evicted_bytes: i64_from_u64(summary.evicted_bytes),
        evicted_session_ids: summary.evicted_session_ids.clone(),
        target_reached: summary.target_reached,
        completed_at: Some(timestamp_from_system_time(summary.completed_at)),
    }
}

fn proto_hls_weak_network_status(snapshot: &HlsWeakNetworkSnapshot) -> HlsWeakNetworkStatus {
    HlsWeakNetworkStatus {
        state: proto_hls_weak_network_state(snapshot.state).into(),
        message: snapshot.message.clone(),
        degraded_session_count: snapshot
            .degraded_session_count
            .try_into()
            .unwrap_or(i32::MAX),
        unhealthy_variant_count: snapshot
            .unhealthy_variant_count
            .try_into()
            .unwrap_or(i32::MAX),
        retrying_variant_count: snapshot
            .retrying_variant_count
            .try_into()
            .unwrap_or(i32::MAX),
        cache_only_session_count: snapshot
            .cache_only_session_count
            .try_into()
            .unwrap_or(i32::MAX),
        last_changed_at: snapshot.last_changed_at.map(timestamp_from_system_time),
    }
}

fn proto_hls_weak_network_state(state: RuntimeHlsWeakNetworkState) -> HlsWeakNetworkState {
    match state {
        RuntimeHlsWeakNetworkState::Normal => HlsWeakNetworkState::Normal,
        RuntimeHlsWeakNetworkState::Retrying => HlsWeakNetworkState::Retrying,
        RuntimeHlsWeakNetworkState::Degraded => HlsWeakNetworkState::Degraded,
        RuntimeHlsWeakNetworkState::CacheOnly => HlsWeakNetworkState::CacheOnly,
        RuntimeHlsWeakNetworkState::UpstreamFailed => HlsWeakNetworkState::UpstreamFailed,
    }
}

fn proto_hls_playback_progress_status(
    snapshot: &HlsPlaybackProgressSnapshot,
) -> HlsPlaybackProgressStatus {
    HlsPlaybackProgressStatus {
        state: proto_hls_playback_activity_state(snapshot.state).into(),
        message: snapshot.message.clone(),
        session_id: snapshot.session_id.clone(),
        library_item_id: snapshot.library_item_id.clone(),
        variant_id: snapshot.variant_id.clone(),
        playback_uri: snapshot.playback_uri.clone(),
        position_seconds: snapshot.position_seconds,
        duration_seconds: snapshot.duration_seconds.unwrap_or_default(),
        last_intent: proto_playback_progress_intent(snapshot.last_intent).into(),
        updated_at: (snapshot.state != HlsPlaybackActivityState::None)
            .then(|| timestamp_from_system_time(snapshot.updated_at)),
    }
}

fn proto_hls_playback_activity_state(
    state: HlsPlaybackActivityState,
) -> ProtoHlsPlaybackActivityState {
    match state {
        HlsPlaybackActivityState::None => ProtoHlsPlaybackActivityState::None,
        HlsPlaybackActivityState::Active => ProtoHlsPlaybackActivityState::Active,
        HlsPlaybackActivityState::RecentlyStopped => ProtoHlsPlaybackActivityState::RecentlyStopped,
    }
}

fn proto_playback_progress_intent(intent: PlaybackProgressIntent) -> ProtoPlaybackProgressIntent {
    match intent {
        PlaybackProgressIntent::Started => ProtoPlaybackProgressIntent::Started,
        PlaybackProgressIntent::Playing => ProtoPlaybackProgressIntent::Playing,
        PlaybackProgressIntent::Seek => ProtoPlaybackProgressIntent::Seek,
        PlaybackProgressIntent::Paused => ProtoPlaybackProgressIntent::Paused,
        PlaybackProgressIntent::Stopped => ProtoPlaybackProgressIntent::Stopped,
    }
}

fn playback_progress_intent_from_proto(
    intent: ProtoPlaybackProgressIntent,
) -> Result<PlaybackProgressIntent, Status> {
    match intent {
        ProtoPlaybackProgressIntent::Started => Ok(PlaybackProgressIntent::Started),
        ProtoPlaybackProgressIntent::Playing => Ok(PlaybackProgressIntent::Playing),
        ProtoPlaybackProgressIntent::Seek => Ok(PlaybackProgressIntent::Seek),
        ProtoPlaybackProgressIntent::Paused => Ok(PlaybackProgressIntent::Paused),
        ProtoPlaybackProgressIntent::Stopped => Ok(PlaybackProgressIntent::Stopped),
        ProtoPlaybackProgressIntent::Unspecified => Err(Status::invalid_argument(
            "Playback progress intent is required.",
        )),
    }
}

fn proto_lan_transcoding_status(snapshot: &LanTranscodingStatusSnapshot) -> LanTranscodingStatus {
    LanTranscodingStatus {
        enabled: snapshot.enabled,
        state: proto_lan_transcoding_runtime_state(snapshot.state).into(),
        message: snapshot.message.clone(),
        profile_id: snapshot.profile.id.clone(),
        target_container: snapshot.profile.target_container.clone(),
        target_video_codec: snapshot.profile.target_video_codec.clone(),
        target_audio_codec: snapshot.profile.target_audio_codec.clone(),
        max_concurrent_jobs: snapshot.max_concurrent_jobs.try_into().unwrap_or(u32::MAX),
        active_job_count: snapshot.active_job_count.try_into().unwrap_or(u32::MAX),
    }
}

fn proto_lan_transcoding_runtime_state(
    state: LanTranscodingRuntimeState,
) -> ProtoLanTranscodingRuntimeState {
    match state {
        LanTranscodingRuntimeState::Disabled => ProtoLanTranscodingRuntimeState::Disabled,
        LanTranscodingRuntimeState::Idle => ProtoLanTranscodingRuntimeState::Idle,
        LanTranscodingRuntimeState::Busy => ProtoLanTranscodingRuntimeState::Busy,
    }
}

fn proto_lan_transcoding_plan(plan: &HlsTranscodingPlan) -> LanTranscodingPlan {
    LanTranscodingPlan {
        state: proto_lan_transcoding_plan_state(plan.state).into(),
        profile_id: plan.profile_id.clone(),
        reason: plan.reason.clone(),
        source_variant_id: plan.source_variant_id.clone(),
        target_container: plan.target_container.clone(),
        target_video_codec: plan.target_video_codec.clone(),
        target_audio_codec: plan.target_audio_codec.clone(),
        output_protocol: match plan.output_protocol.as_str() {
            "hls" => PlaybackProtocol::Hls.into(),
            _ => PlaybackProtocol::Unspecified.into(),
        },
    }
}

fn proto_lan_transcoding_plan_state(state: HlsTranscodingPlanState) -> LanTranscodingPlanState {
    match state {
        HlsTranscodingPlanState::Disabled => LanTranscodingPlanState::Disabled,
        HlsTranscodingPlanState::NotRequired => LanTranscodingPlanState::NotRequired,
        HlsTranscodingPlanState::Ready => LanTranscodingPlanState::Ready,
        HlsTranscodingPlanState::Unsupported => LanTranscodingPlanState::Unsupported,
    }
}

fn i64_from_u64(value: u64) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
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
    selection_plan: BilibiliPlaybackSelectionPlan,
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
                .complete_task_cancelled(&task_id, PLAYBACK_PLANNING_CANCELLED_MESSAGE.to_owned())
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
            .complete_task_cancelled(&task_id, PLAYBACK_PLANNING_CANCELLED_MESSAGE.to_owned())
            .is_ok()
        {
            cleanup.disarm();
        }
        return;
    }
    let completed = match selection_plan.mode.clone() {
        BilibiliPlaybackSelectionPlanMode::LegacySingle { selection_id } => {
            run_single_bilibili_playback_planning(
                state,
                task_id,
                source,
                options,
                selection_id,
                playback_source_uri,
                cancellation,
            )
            .await
        }
        _ => {
            run_explicit_bilibili_playback_planning(
                state,
                task_id,
                source,
                options,
                selection_plan,
                playback_source_uri,
                cancellation,
            )
            .await
        }
    };
    if completed {
        cleanup.disarm();
    }
}

async fn run_single_bilibili_playback_planning(
    state: AppState,
    task_id: String,
    source: String,
    options: Option<BilibiliPlaybackOptions>,
    selection_id: Option<String>,
    playback_source_uri: String,
    cancellation: crate::task_registry::BilibiliTaskCancellation,
) -> bool {
    let playback_policy = PlaybackPolicy::from_playback_options(options.as_ref());
    let planning_request = BilibiliPlaybackPlanningRequest {
        source,
        options,
        selection_id,
        cancellation,
    };
    let plan = match state.playback_planner.plan(planning_request).await {
        Ok(plan) => plan,
        Err(error) => {
            let message = playback_error_message(error);
            return state
                .tasks
                .complete_task_failed(&task_id, state.error_detail_for_client(&message))
                .is_ok();
        }
    };
    let metadata =
        match playback_task_metadata_with_policy(&task_id, plan, &state.options, playback_policy) {
            Ok(metadata) => metadata,
            Err(error) => {
                return state
                    .tasks
                    .complete_task_failed(&task_id, state.error_detail_for_client(&error.message()))
                    .is_ok();
            }
        };

    let playback_source = PlaybackSource {
        item_id: task_id.clone(),
        variant_id: metadata.playback_session.selected_variant_id.clone(),
        protocol: PlaybackProtocol::Hls.into(),
        uri: playback_source_uri,
        expires_at: None,
    };
    state.register_hls_playback_session(metadata.hls_session.clone());
    match state.tasks.complete_playback_playable(
        &task_id,
        metadata.title,
        playback_source,
        metadata.playback_session,
    ) {
        Ok(task) => {
            if task.state() != TaskState::Playable {
                state.remove_hls_playback_session(&task_id);
                let _ = state.hls_cache.remove_session(&task_id);
            } else {
                if let Err(error) = state.hls_cache.save_session(&metadata.hls_session) {
                    eprintln!(
                        "Failed to persist HLS playback manifest for task {task_id}; keeping runtime playback source available: {}",
                        state.error_detail_for_log(&error)
                    );
                }
                state.enqueue_hls_cache_fill_foreground(
                    task_id.clone(),
                    metadata.hls_session,
                    HlsCacheFinalizationFailureMode::KeepPlayable,
                );
            }
            true
        }
        Err(_) => {
            state.remove_hls_playback_session(&task_id);
            let _ = state.hls_cache.remove_session(&task_id);
            false
        }
    }
}

async fn run_explicit_bilibili_playback_planning(
    state: AppState,
    task_id: String,
    source: String,
    options: Option<BilibiliPlaybackOptions>,
    selection_plan: BilibiliPlaybackSelectionPlan,
    primary_playback_source_uri: String,
    cancellation: crate::task_registry::BilibiliTaskCancellation,
) -> bool {
    let playback_policy = PlaybackPolicy::from_playback_options(options.as_ref());
    let resolution = match state
        .playback_planner
        .resolve_input(BilibiliInputResolveRequest {
            source: source.clone(),
            options: options.clone(),
            cancellation: cancellation.clone(),
        })
        .await
    {
        Ok(resolution) => resolution,
        Err(error) if cancellation.is_cancel_requested() => {
            let message = playback_error_message(error);
            return state
                .tasks
                .complete_task_cancelled(&task_id, state.cancellation_detail_for_client(&message))
                .is_ok();
        }
        Err(error) => {
            let message = playback_error_message(error);
            return state
                .tasks
                .complete_task_failed(&task_id, state.error_detail_for_client(&message))
                .is_ok();
        }
    };
    let candidates = match selected_bilibili_candidates(&resolution, &selection_plan.mode) {
        Ok(candidates) => candidates,
        Err(message) => {
            return state.tasks.complete_task_failed(&task_id, message).is_ok();
        }
    };
    let total = candidates.len();
    let mut result_items = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            bilibili_result_item(
                result_session_id(&task_id, index),
                candidate,
                TaskState::Preparing,
                BILIBILI_RESULT_PLANNING_MESSAGE.to_owned(),
            )
        })
        .collect::<Vec<_>>();
    let _ = state.tasks.update_playback_results(
        &task_id,
        Some(resolution.title.clone()),
        format!("Planning {total} Bilibili playback result(s)."),
        0.0,
        result_items.clone(),
    );

    let mut primary: Option<(
        PlaybackSource,
        BilibiliPlaybackSession,
        HlsPlaybackSession,
        String,
    )> = None;
    let mut successful_results = 0_usize;
    let mut planned_session_ids = Vec::new();
    let mut planned_sessions = Vec::new();

    for (index, candidate) in candidates.iter().enumerate() {
        if cancellation.is_cancel_requested() {
            return complete_cancelled_explicit_bilibili_playback(
                &state,
                &task_id,
                &resolution.title,
                &mut result_items,
                &planned_session_ids,
            );
        }

        let session_id = result_items[index].id.clone();
        let planning_request = BilibiliPlaybackPlanningRequest {
            source: source.clone(),
            options: options.clone(),
            selection_id: Some(candidate.selection_id.clone()),
            cancellation: cancellation.clone(),
        };
        let item_outcome = match state.playback_planner.plan(planning_request).await {
            Ok(plan) => playback_task_metadata_with_policy(
                &session_id,
                plan,
                &state.options,
                playback_policy,
            )
            .map_err(|error| BilibiliDownloadError::Failed(error.message().to_owned())),
            Err(error) => Err(error),
        };

        match item_outcome {
            Ok(metadata) => {
                let playback_source_uri = if index == 0 {
                    primary_playback_source_uri.clone()
                } else {
                    related_hls_master_playlist_uri(
                        &primary_playback_source_uri,
                        &task_id,
                        &session_id,
                    )
                };
                let playback_source = PlaybackSource {
                    item_id: session_id.clone(),
                    variant_id: metadata.playback_session.selected_variant_id.clone(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: playback_source_uri,
                    expires_at: None,
                };
                result_items[index].title = metadata.title.clone();
                result_items[index].state = TaskState::Playable.into();
                result_items[index].message = BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned();
                result_items[index].playback_source = Some(playback_source.clone());
                result_items[index].playback_session = Some(metadata.playback_session.clone());
                state.register_hls_playback_session(metadata.hls_session.clone());
                planned_session_ids.push(session_id.clone());
                planned_sessions.push(metadata.hls_session.clone());
                if let Err(error) = state.hls_cache.save_session(&metadata.hls_session) {
                    eprintln!(
                        "Failed to persist HLS playback manifest for result {session_id}; keeping runtime playback source available: {}",
                        state.error_detail_for_log(&error)
                    );
                }
                if primary.is_none() {
                    let mut primary_playback_source = playback_source.clone();
                    primary_playback_source.item_id = task_id.clone();
                    primary = Some((
                        primary_playback_source,
                        metadata.playback_session,
                        metadata.hls_session,
                        metadata.title,
                    ));
                }
                successful_results += 1;
            }
            Err(error) if cancellation.is_cancel_requested() => {
                eprintln!(
                    "Bilibili playback planning for task {task_id} observed cancellation after planner error: {}",
                    state.error_detail_for_log(&playback_error_message(error))
                );
                return complete_cancelled_explicit_bilibili_playback(
                    &state,
                    &task_id,
                    &resolution.title,
                    &mut result_items,
                    &planned_session_ids,
                );
            }
            Err(error) => {
                result_items[index].state = TaskState::Failed.into();
                let message = playback_error_message(error);
                result_items[index].message = state.error_detail_for_client(&message);
            }
        }

        let message = format!(
            "Planned {}/{} Bilibili playback result(s).",
            index + 1,
            total
        );
        let _ = state.tasks.update_playback_results(
            &task_id,
            Some(resolution.title.clone()),
            message,
            result_items_progress(&result_items),
            result_items.clone(),
        );
    }

    let Some((primary_source, primary_session, primary_hls_session, primary_title)) = primary
    else {
        return state
            .tasks
            .complete_task_failed(
                &task_id,
                "Failed to plan any selected Bilibili playback result.".to_owned(),
            )
            .is_ok();
    };
    if cancellation.is_cancel_requested() {
        return complete_cancelled_explicit_bilibili_playback(
            &state,
            &task_id,
            &resolution.title,
            &mut result_items,
            &planned_session_ids,
        );
    }

    let final_message = if successful_results == total {
        format!("All {total} Bilibili playback result(s) are playable.")
    } else {
        format!("{successful_results}/{total} Bilibili playback result(s) are playable.")
    };
    if let Some(first_item) = result_items
        .iter_mut()
        .find(|item| item.id == primary_hls_session.id)
    {
        first_item.message = final_message.clone();
    }

    match state.tasks.complete_playback_results_playable(
        &task_id,
        primary_title,
        final_message,
        primary_source,
        primary_session,
        result_items,
    ) {
        Ok(task) => {
            if task.state() != TaskState::Playable {
                remove_hls_sessions(&state, &planned_session_ids);
            } else {
                let primary_session_id = primary_hls_session.id.clone();
                state.enqueue_hls_cache_fill_foreground(
                    task_id.clone(),
                    primary_hls_session,
                    HlsCacheFinalizationFailureMode::KeepPlayable,
                );
                for session in planned_sessions {
                    if session.id == primary_session_id {
                        continue;
                    }
                    state.enqueue_hls_cache_fill_demoted(
                        task_id.clone(),
                        session,
                        HlsCacheFinalizationFailureMode::KeepPlayable,
                    );
                }
            }
            true
        }
        Err(_) => {
            remove_hls_sessions(&state, &planned_session_ids);
            false
        }
    }
}

fn complete_cancelled_explicit_bilibili_playback(
    state: &AppState,
    task_id: &str,
    title: &str,
    result_items: &mut [BilibiliTaskResultItem],
    planned_session_ids: &[String],
) -> bool {
    mark_results_cancelled(result_items);
    remove_hls_sessions(state, planned_session_ids);
    let _ = state.tasks.update_playback_results(
        task_id,
        Some(title.to_owned()),
        PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE.to_owned(),
        result_items_progress(result_items),
        result_items.to_vec(),
    );
    state
        .tasks
        .complete_task_cancelled(
            task_id,
            PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE.to_owned(),
        )
        .is_ok()
}

fn selected_bilibili_candidates(
    resolution: &BilibiliInputResolution,
    mode: &BilibiliPlaybackSelectionPlanMode,
) -> Result<Vec<AdapterBilibiliResolvedCandidate>, String> {
    match mode {
        BilibiliPlaybackSelectionPlanMode::ExplicitIds { selection_ids } => selection_ids
            .iter()
            .map(|selection_id| {
                resolution
                    .candidates
                    .iter()
                    .find(|candidate| candidate.selection_id == *selection_id)
                    .cloned()
                    .or_else(|| {
                        recover_stable_collection_candidate(
                            selection_id,
                            &resolution.source,
                            &resolution.source_kind,
                            &resolution.candidates,
                        )
                    })
                    .ok_or_else(|| {
                        format!(
                            "Selected Bilibili item {selection_id:?} was not found. Resolve the input again and retry."
                        )
                    })
            })
            .collect(),
        BilibiliPlaybackSelectionPlanMode::ExplicitRange {
            start_index,
            end_index,
        } => {
            let expected_count = end_index.saturating_sub(*start_index) + 1;
            let candidates = resolution
                .candidates
                .iter()
                .filter(|candidate| {
                    candidate.index >= *start_index && candidate.index <= *end_index
                })
                .cloned()
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                Err(format!(
                    "Bilibili task selection range {start_index}-{end_index} did not match any resolved items."
                ))
            } else if u32::try_from(candidates.len()).unwrap_or(u32::MAX) != expected_count {
                Err(format!(
                    "Bilibili task selection range {start_index}-{end_index} could not be fully resolved. Resolve the input again or choose a smaller range."
                ))
            } else {
                Ok(candidates)
            }
        }
        BilibiliPlaybackSelectionPlanMode::ExplicitAll => {
            if resolution.candidates.is_empty() {
                Err("Bilibili task selection did not resolve any items.".to_owned())
            } else if resolution.candidates_truncated {
                Err(
                    "All Bilibili task selection is unavailable because the resolved item list is truncated. Choose explicit ids or a smaller range."
                        .to_owned(),
                )
            } else {
                Ok(resolution.candidates.clone())
            }
        }
        BilibiliPlaybackSelectionPlanMode::LegacySingle { .. } => {
            Err("Legacy Bilibili playback selection does not require resolved candidates.".to_owned())
        }
    }
}

fn result_session_id(task_id: &str, index: usize) -> String {
    if index == 0 {
        task_id.to_owned()
    } else {
        format!("{task_id}-result-{}", index + 1)
    }
}

fn bilibili_result_item(
    id: String,
    candidate: &AdapterBilibiliResolvedCandidate,
    state: TaskState,
    message: String,
) -> BilibiliTaskResultItem {
    BilibiliTaskResultItem {
        id,
        selection_id: candidate.selection_id.clone(),
        title: candidate.title.clone(),
        subtitle: candidate.subtitle.clone(),
        source_kind: candidate.source_kind.clone(),
        content_id: candidate.content_id.clone(),
        index: candidate.index,
        state: state.into(),
        message,
        library_item_id: String::new(),
        playback_source: None,
        playback_session: None,
    }
}

fn remove_hls_sessions(state: &AppState, session_ids: &[String]) {
    for session_id in session_ids {
        state.remove_hls_playback_session(session_id);
        let _ = state.hls_cache.remove_session(session_id);
    }
}

fn mark_results_cancelled(items: &mut [BilibiliTaskResultItem]) {
    for item in items {
        item.state = TaskState::Cancelled.into();
        item.message = PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE.to_owned();
        item.library_item_id.clear();
        item.playback_source = None;
        item.playback_session = None;
    }
}

fn result_items_progress(items: &[BilibiliTaskResultItem]) -> f64 {
    if items.is_empty() {
        return 0.0;
    }
    let finished = items
        .iter()
        .filter(|item| {
            matches!(
                TaskState::try_from(item.state).unwrap_or(TaskState::Unspecified),
                TaskState::Playable
                    | TaskState::Completed
                    | TaskState::Failed
                    | TaskState::Cancelled
            )
        })
        .count();
    (finished as f64 / items.len() as f64).clamp(0.0, 1.0)
}

fn related_hls_master_playlist_uri(
    primary_uri: &str,
    primary_session_id: &str,
    session_id: &str,
) -> String {
    let primary_suffix = format!(
        "/hls/{}/master.m3u8",
        urlencoding::encode(primary_session_id)
    );
    if let Some(prefix) = primary_uri.strip_suffix(&primary_suffix) {
        return format!(
            "{prefix}/hls/{}/master.m3u8",
            urlencoding::encode(session_id)
        );
    }

    primary_uri.to_owned()
}

#[cfg(test)]
pub(crate) async fn run_hls_cache_finalization(
    state: AppState,
    task_id: String,
    session: HlsPlaybackSession,
    failure_mode: HlsCacheFinalizationFailureMode,
) {
    let _ = run_hls_cache_finalization_inner(
        state,
        task_id,
        session,
        failure_mode,
        HlsFillPreemptionToken::default(),
    )
    .await;
}

pub(crate) async fn run_hls_cache_fill_worker(state: AppState) {
    let _worker_guard = state.hls_fill_scheduler.worker_guard();
    while let Some(job) = state.hls_fill_scheduler.next_job_until_shutdown().await {
        let session_id = job.session.id.clone();
        let outcome = run_hls_cache_finalization_inner(
            state.clone(),
            job.task_id.clone(),
            job.session.clone(),
            job.failure_mode,
            job.token.clone(),
        )
        .await;
        let should_requeue = outcome == HlsCacheFinalizationOutcome::Preempted
            && state
                .tasks
                .is_hls_session_playable_for_task(&job.task_id, &session_id);
        if should_requeue {
            let message = match job.priority {
                crate::hls_fill_scheduler::HlsFillPriority::Foreground => {
                    "Playable online; offline cache fill paused behind newer playback."
                }
                crate::hls_fill_scheduler::HlsFillPriority::Demoted => {
                    "Playable online; offline cache fill remains queued behind newer playback."
                }
            };
            let _ = state.tasks.update_playback_cache_progress(
                &job.task_id,
                BilibiliTaskProgress {
                    progress: None,
                    downloaded_bytes: None,
                    total_bytes: None,
                    message: Some(message.to_owned()),
                },
            );
        }
        state
            .hls_fill_scheduler
            .finish_current(&job, should_requeue);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsCacheFinalizationOutcome {
    Finished,
    Preempted,
}

async fn run_hls_cache_finalization_inner(
    state: AppState,
    task_id: String,
    session: HlsPlaybackSession,
    failure_mode: HlsCacheFinalizationFailureMode,
    preemption: HlsFillPreemptionToken,
) -> HlsCacheFinalizationOutcome {
    if !state.supports_completed_hls_cache_playback() {
        return HlsCacheFinalizationOutcome::Finished;
    }
    let session_id = session.id.clone();
    let permit_request = Arc::clone(&state.hls_cache_finalization_permits).acquire_owned();
    tokio::pin!(permit_request);
    let _permit = loop {
        if preemption.is_cancelled() {
            return HlsCacheFinalizationOutcome::Finished;
        }
        if !state
            .tasks
            .is_hls_session_playable_for_task(&task_id, &session_id)
        {
            return HlsCacheFinalizationOutcome::Finished;
        }
        if preemption.is_preempted() {
            return HlsCacheFinalizationOutcome::Preempted;
        }
        tokio::select! {
            permit = &mut permit_request => {
                match permit {
                    Ok(permit) => break permit,
                    Err(_) => {
                        eprintln!(
                            "HLS cache finalization limiter is unavailable for task {task_id}."
                        );
                        return HlsCacheFinalizationOutcome::Finished;
                    }
                }
            }
            () = sleep(Duration::from_millis(100)) => {}
        }
    };
    let _eviction_protection = state.protect_hls_cache_session_from_eviction(&session_id);
    let control = || {
        if preemption.is_cancelled()
            || !state
                .tasks
                .is_hls_session_playable_for_task(&task_id, &session_id)
        {
            HlsCacheFillControl::Cancel
        } else if preemption.is_preempted() {
            HlsCacheFillControl::Preempt
        } else {
            HlsCacheFillControl::Continue
        }
    };
    if control() == HlsCacheFillControl::Preempt {
        return HlsCacheFinalizationOutcome::Preempted;
    }
    if control() == HlsCacheFillControl::Cancel {
        return HlsCacheFinalizationOutcome::Finished;
    }
    let playback_progress = state.hls_playback_progress_for_session(&session_id);
    let _ = state.tasks.update_playback_cache_progress(
        &task_id,
        BilibiliTaskProgress {
            progress: Some(0.0),
            downloaded_bytes: Some(0),
            total_bytes: hls_session_declared_size_bytes(&session)
                .map(|value| value.try_into().unwrap_or(i64::MAX)),
            message: Some(hls_cache_prewarm_progress_message(
                playback_progress.as_ref(),
            )),
        },
    );
    match state
        .hls_cache
        .prewarm_session_first_frame_with_playback_progress(
            &state.hls_upstream_client,
            &session,
            playback_progress.as_ref(),
            &control,
        )
        .await
    {
        Ok(()) => {}
        Err(crate::hls_cache::HlsCacheError::Preempted) => {
            return HlsCacheFinalizationOutcome::Preempted;
        }
        Err(crate::hls_cache::HlsCacheError::Cancelled) => {
            state.remove_hls_playback_session(&session_id);
            let _ = state.hls_cache.remove_session(&session_id);
            return HlsCacheFinalizationOutcome::Finished;
        }
        Err(error) => {
            eprintln!(
                "Failed to prewarm HLS playback cache for task {task_id}; continuing full cache fill: {}",
                state.error_detail_for_log(&error)
            );
        }
    }
    if control() == HlsCacheFillControl::Preempt {
        return HlsCacheFinalizationOutcome::Preempted;
    }
    if control() == HlsCacheFillControl::Cancel {
        return HlsCacheFinalizationOutcome::Finished;
    }
    let projected_added_bytes = state
        .hls_cache
        .session_projected_finalization_added_size_bytes(&session)
        .unwrap_or_default();
    if let Err(error) = state.enforce_hls_cache_quota_until_cancelled(
        "before_hls_finalization",
        [session_id.clone()],
        projected_added_bytes,
        || control() != HlsCacheFillControl::Continue,
    ) {
        eprintln!(
            "Failed to run HLS cache eviction before finalization for task {task_id}: {}",
            state.error_detail_for_log(&error)
        );
    }
    if control() == HlsCacheFillControl::Preempt {
        return HlsCacheFinalizationOutcome::Preempted;
    }
    if control() == HlsCacheFillControl::Cancel {
        return HlsCacheFinalizationOutcome::Finished;
    }
    let progress = hls_cache_progress_reporter(&state, &task_id);
    match state
        .hls_cache
        .cache_session_resources_completion_with_control(
            &state.hls_upstream_client,
            &session,
            control,
            progress,
            state.hls_transcoding_execution_config(),
        )
        .await
    {
        Ok(completion) => {
            let completed_playback_session =
                playback_session_from_hls_cache_session(&completion.session);
            let library_item_id = completion.library_item_id.clone();
            let finalized = state
                .tasks
                .complete_playback_hls_session_cached_with_metadata(
                    &task_id,
                    &session_id,
                    library_item_id.clone(),
                    completed_playback_session,
                );
            match finalized {
                Ok(task)
                    if state.tasks.playback_task_has_completed_hls_cache_item(
                        &task,
                        &session_id,
                        &library_item_id,
                    ) =>
                {
                    state.register_completed_hls_runtime_session(&completion.session);
                    if let Err(error) = state.enforce_hls_cache_quota(
                        "after_hls_finalization",
                        [session_id.clone()],
                        0,
                    ) {
                        eprintln!(
                            "Failed to run HLS cache eviction after finalization for task {task_id}: {}",
                            state.error_detail_for_log(&error)
                        );
                    }
                }
                Ok(_) | Err(_) => {
                    state.remove_hls_playback_session(&session_id);
                    let _ = state.hls_cache.remove_session(&session_id);
                }
            }
        }
        Err(crate::hls_cache::HlsCacheError::Cancelled) => {
            state.remove_hls_playback_session(&session_id);
            let _ = state.hls_cache.remove_session(&session_id);
        }
        Err(crate::hls_cache::HlsCacheError::Preempted) => {
            return HlsCacheFinalizationOutcome::Preempted;
        }
        Err(error) => {
            if !state
                .tasks
                .is_hls_session_playable_for_task(&task_id, &session_id)
            {
                return HlsCacheFinalizationOutcome::Finished;
            }
            match failure_mode {
                HlsCacheFinalizationFailureMode::KeepPlayable => {
                    eprintln!(
                        "Failed to finalize HLS playback cache for task {task_id}; keeping runtime playback source available: {}",
                        state.error_detail_for_log(&error)
                    );
                    if let Err(status) = state.tasks.fail_hls_cache_fill_for_playback_session(
                        &task_id,
                        &session_id,
                        state.error_with_context_for_client(
                            "Playable online; offline cache fill failed",
                            &error,
                        ),
                    ) {
                        eprintln!(
                            "Failed to publish HLS cache fill failure for task {task_id} session {session_id}: {}",
                            state.error_detail_for_log(&status)
                        );
                    }
                }
                HlsCacheFinalizationFailureMode::FailRestoredTask => {
                    state.remove_hls_playback_session(&session_id);
                    let _ = state.hls_cache.remove_session(&session_id);
                    if let Err(status) = state
                        .tasks
                        .fail_unrestorable_playback_session_after_cache_restore(
                            &session_id,
                            state.error_with_context_for_client(
                                "Failed to restore offline HLS cache after restart",
                                &error,
                            ),
                        )
                    {
                        eprintln!(
                            "Failed to mark restored HLS playback task {task_id} failed after cache finalization error: {}",
                            state.error_detail_for_log(&status)
                        );
                    }
                }
            }
        }
    }
    HlsCacheFinalizationOutcome::Finished
}

fn hls_cache_prewarm_progress_message(
    playback_progress: Option<&HlsPlaybackProgressSnapshot>,
) -> String {
    if let Some(snapshot) = playback_progress
        && snapshot.state == HlsPlaybackActivityState::Active
        && snapshot.position_seconds.is_finite()
        && snapshot.position_seconds > 0.0
    {
        return format!(
            "Playable online; prefetching HLS cache near {:.0}s playback position.",
            snapshot.position_seconds
        );
    }

    "Playable online; prefetching first playback window.".to_owned()
}

fn hls_cache_progress_reporter(
    state: &AppState,
    task_id: &str,
) -> impl Fn(HlsCacheFillProgress) + Send + Sync + 'static {
    let tasks = Arc::clone(&state.tasks);
    let task_id = task_id.to_owned();
    let last_published_bytes = Arc::new(std::sync::Mutex::new(None::<u64>));
    move |progress| {
        let should_publish = {
            let mut last_published_bytes = last_published_bytes
                .lock()
                .expect("HLS cache progress lock poisoned");
            let downloaded_bytes = progress.downloaded_bytes;
            let total_bytes = progress.total_bytes.unwrap_or_default();
            let should_publish = last_published_bytes.is_none()
                || Some(downloaded_bytes) == progress.total_bytes
                || last_published_bytes.as_ref().is_some_and(|last| {
                    downloaded_bytes.saturating_sub(*last) >= HLS_CACHE_PROGRESS_PUBLISH_MIN_BYTES
                });
            if should_publish {
                *last_published_bytes = Some(downloaded_bytes);
            }
            should_publish || downloaded_bytes == 0 || downloaded_bytes == total_bytes
        };
        if !should_publish {
            return;
        }

        let progress_value = progress.total_bytes.and_then(|total_bytes| {
            (total_bytes > 0)
                .then(|| (progress.downloaded_bytes as f64 / total_bytes as f64).clamp(0.0, 0.99))
        });
        let message = match progress_value {
            Some(value) if value > 0.0 => format!(
                "Playable online; filling offline cache ({:.0}%).",
                value * 100.0
            ),
            _ => "Playable online; filling offline cache in background.".to_owned(),
        };
        let _ = tasks.update_playback_cache_progress(
            &task_id,
            BilibiliTaskProgress {
                progress: progress_value,
                downloaded_bytes: Some(progress.downloaded_bytes.try_into().unwrap_or(i64::MAX)),
                total_bytes: progress
                    .total_bytes
                    .map(|value| value.try_into().unwrap_or(i64::MAX)),
                message: Some(message),
            },
        );
    }
}

struct PlaybackTaskMetadata {
    title: String,
    playback_session: BilibiliPlaybackSession,
    hls_session: HlsPlaybackSession,
}

#[cfg(test)]
fn playback_task_metadata(
    task_id: &str,
    plan: BilibiliPlaybackPlan,
) -> Result<PlaybackTaskMetadata, Status> {
    playback_task_metadata_with_options(task_id, plan, &CacheServerOptions::default())
}

#[cfg(test)]
fn playback_task_metadata_with_options(
    task_id: &str,
    plan: BilibiliPlaybackPlan,
    options: &CacheServerOptions,
) -> Result<PlaybackTaskMetadata, Status> {
    playback_task_metadata_with_policy(task_id, plan, options, PlaybackPolicy::default())
}

fn playback_task_metadata_with_policy(
    task_id: &str,
    plan: BilibiliPlaybackPlan,
    options: &CacheServerOptions,
    playback_policy: PlaybackPolicy,
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
    let mut hls_session = HlsPlaybackSession::from_playback_entry_with_policy(
        task_id,
        &title,
        &selected.variant,
        &entry.abr,
        &entry.variants,
        playback_policy,
    )
    .map_err(|error| Status::failed_precondition(error.to_string()))?;
    hls_session.transcoding =
        HlsTranscodingPlan::for_variant_with_policy(options, &selected.variant, playback_policy);
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
        transcoding_plan: Some(proto_lan_transcoding_plan(&hls_session.transcoding)),
        effective_policy: Some(playback_policy.to_proto()),
    };

    Ok(PlaybackTaskMetadata {
        title,
        playback_session,
        hls_session,
    })
}

pub(crate) fn playback_session_from_hls_cache_session(
    session: &HlsPlaybackSession,
) -> BilibiliPlaybackSession {
    let selected_variant = playback_variant_from_hls_variant(&session.variant);
    let mut variants = session
        .variants
        .iter()
        .map(playback_variant_from_hls_metadata)
        .collect::<Vec<_>>();
    if variants.is_empty()
        || !variants
            .iter()
            .any(|variant| variant.id == selected_variant.id)
    {
        variants.push(selected_variant.clone());
    }

    BilibiliPlaybackSession {
        id: session.id.clone(),
        title: session.title.clone(),
        content_id: session
            .variants
            .iter()
            .find(|variant| variant.id == session.variant.id)
            .map(|variant| variant.content_id.clone())
            .filter(|content_id| !content_id.trim().is_empty())
            .unwrap_or_else(|| session.variant.video.request.cache_key.content_id.clone()),
        selected_variant_id: session.variant.id.clone(),
        selected_variant: Some(selected_variant),
        variants,
        transcoding_plan: Some(proto_lan_transcoding_plan(&session.transcoding)),
        effective_policy: Some(session.effective_policy.to_proto()),
    }
}

fn playback_variant_from_hls_variant(variant: &HlsVariant) -> BilibiliPlaybackVariant {
    BilibiliPlaybackVariant {
        id: variant.id.clone(),
        label: hls_variant_label(variant.id.as_str(), variant.width, variant.height),
        source_kind: playback_variant_kind_name(BilibiliPlaybackVariantKind::Dash).to_owned(),
        container: playback_variant_container(BilibiliPlaybackVariantKind::Dash).to_owned(),
        video_codec: hls_variant_video_codec(variant),
        audio_codec: hls_variant_audio_codec(variant),
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
        bitrate: variant.bandwidth.try_into().unwrap_or(i64::MAX),
        size_bytes: hls_variant_size_bytes(variant)
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i64::MAX),
    }
}

fn playback_variant_from_hls_metadata(metadata: &HlsVariantMetadata) -> BilibiliPlaybackVariant {
    BilibiliPlaybackVariant {
        id: metadata.id.clone(),
        label: hls_variant_label(metadata.id.as_str(), metadata.width, metadata.height),
        source_kind: playback_variant_kind_name(metadata.kind).to_owned(),
        container: playback_variant_container(metadata.kind).to_owned(),
        video_codec: hls_metadata_video_codec(metadata),
        audio_codec: hls_metadata_audio_codec(metadata),
        width: metadata
            .width
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i32::MAX),
        height: metadata
            .height
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i32::MAX),
        bitrate: metadata
            .bandwidth
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i64::MAX),
        size_bytes: hls_metadata_size_bytes(metadata)
            .unwrap_or_default()
            .try_into()
            .unwrap_or(i64::MAX),
    }
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

fn hls_variant_label(id: &str, width: Option<u32>, height: Option<u32>) -> String {
    match (width, height) {
        (Some(width), Some(height)) => format!("{width}x{height}"),
        _ => id.to_owned(),
    }
}

fn hls_variant_video_codec(variant: &HlsVariant) -> String {
    first_matching_codec(variant.codecs.iter().map(String::as_str), is_video_codec)
        .or_else(|| first_matching_codec(variant.video.request.codecs.as_deref(), is_video_codec))
        .unwrap_or_default()
}

fn hls_variant_audio_codec(variant: &HlsVariant) -> String {
    variant
        .audio
        .as_ref()
        .and_then(|audio| first_matching_codec(audio.request.codecs.as_deref(), is_audio_codec))
        .or_else(|| first_matching_codec(variant.codecs.iter().map(String::as_str), is_audio_codec))
        .or_else(|| first_matching_codec(variant.video.request.codecs.as_deref(), is_audio_codec))
        .unwrap_or_default()
}

fn hls_variant_size_bytes(variant: &HlsVariant) -> Option<u64> {
    let mut total = 0_u64;
    let mut found = false;
    for resource in std::iter::once(&variant.video).chain(variant.audio.iter()) {
        if let Some(size_bytes) = resource.request.size {
            total = total.saturating_add(size_bytes);
            found = true;
        }
    }
    found.then_some(total)
}

fn hls_metadata_video_codec(metadata: &HlsVariantMetadata) -> String {
    first_matching_codec(metadata.codecs.iter().map(String::as_str), is_video_codec)
        .or_else(|| {
            first_matching_codec(
                metadata
                    .media
                    .iter()
                    .filter_map(|media| media.codecs.as_deref()),
                is_video_codec,
            )
        })
        .unwrap_or_default()
}

fn hls_metadata_audio_codec(metadata: &HlsVariantMetadata) -> String {
    first_matching_codec(metadata.codecs.iter().map(String::as_str), is_audio_codec)
        .or_else(|| {
            first_matching_codec(
                metadata
                    .media
                    .iter()
                    .filter_map(|media| media.codecs.as_deref()),
                is_audio_codec,
            )
        })
        .unwrap_or_default()
}

fn hls_metadata_size_bytes(metadata: &HlsVariantMetadata) -> Option<u64> {
    let mut total = 0_u64;
    let mut found = false;
    for media in &metadata.media {
        if let Some(size_bytes) = media.size {
            total = total.saturating_add(size_bytes);
            found = true;
        }
    }
    found.then_some(total)
}

fn first_matching_codec<'a>(
    values: impl IntoIterator<Item = &'a str>,
    predicate: impl Fn(&str) -> bool,
) -> Option<String> {
    for value in values {
        for codec in value.split(',') {
            let codec = codec.trim();
            if !codec.is_empty() && predicate(codec) {
                return Some(codec.to_owned());
            }
        }
    }
    None
}

fn is_audio_codec(codec: &str) -> bool {
    codec.trim().to_ascii_lowercase().starts_with("mp4a.")
}

fn is_video_codec(codec: &str) -> bool {
    !is_audio_codec(codec)
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

fn playback_status_from_error(state: &AppState, error: BilibiliDownloadError) -> Status {
    match error {
        BilibiliDownloadError::Failed(message) => {
            Status::failed_precondition(state.error_detail_for_client(&message))
        }
        BilibiliDownloadError::Cancelled(message) => {
            Status::cancelled(state.cancellation_detail_for_client(&message))
        }
    }
}

fn normalized_optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[derive(Clone, Debug, PartialEq)]
struct BilibiliPlaybackSelectionPlan {
    task_selection: Option<BilibiliTaskSelection>,
    mode: BilibiliPlaybackSelectionPlanMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BilibiliPlaybackSelectionPlanMode {
    LegacySingle { selection_id: Option<String> },
    ExplicitIds { selection_ids: Vec<String> },
    ExplicitRange { start_index: u32, end_index: u32 },
    ExplicitAll,
}

fn playback_selection_plan(
    legacy_selection_id: Option<String>,
    selection: Option<BilibiliTaskSelection>,
) -> Result<BilibiliPlaybackSelectionPlan, Status> {
    let selection = selection.map(normalized_bilibili_task_selection);

    if let Some(selection_id) = legacy_selection_id {
        if let Some(selection) = selection.as_ref()
            && !selection_is_default_empty(selection)
        {
            return Err(Status::invalid_argument(
                "Use either selection_id or selection, not both.",
            ));
        }
        return Ok(BilibiliPlaybackSelectionPlan {
            task_selection: Some(BilibiliTaskSelection {
                mode: BILIBILI_TASK_SELECTION_MODE_SINGLE,
                selection_ids: vec![selection_id.clone()],
                range_start_index: 0,
                range_end_index: 0,
            }),
            mode: BilibiliPlaybackSelectionPlanMode::LegacySingle {
                selection_id: Some(selection_id),
            },
        });
    }

    let Some(selection) = selection else {
        return Ok(BilibiliPlaybackSelectionPlan {
            task_selection: None,
            mode: BilibiliPlaybackSelectionPlanMode::LegacySingle { selection_id: None },
        });
    };

    let has_payload = selection
        .selection_ids
        .iter()
        .any(|selection_id| !selection_id.is_empty())
        || selection.range_start_index != 0
        || selection.range_end_index != 0;

    match selection.mode {
        BILIBILI_TASK_SELECTION_MODE_UNSPECIFIED | BILIBILI_TASK_SELECTION_MODE_DEFAULT => {
            if has_payload {
                return Err(Status::invalid_argument(
                    "Default Bilibili task selection cannot include ids or a range.",
                ));
            }
            Ok(BilibiliPlaybackSelectionPlan {
                task_selection: Some(selection),
                mode: BilibiliPlaybackSelectionPlanMode::LegacySingle { selection_id: None },
            })
        }
        BILIBILI_TASK_SELECTION_MODE_CURRENT => {
            if has_payload {
                return Err(Status::invalid_argument(
                    "Current Bilibili task selection cannot include ids or a range.",
                ));
            }
            Ok(BilibiliPlaybackSelectionPlan {
                task_selection: Some(selection),
                mode: BilibiliPlaybackSelectionPlanMode::LegacySingle { selection_id: None },
            })
        }
        BILIBILI_TASK_SELECTION_MODE_SINGLE => {
            require_no_range(&selection)?;
            if selection.selection_ids.len() != 1 {
                return Err(Status::invalid_argument(
                    "Single Bilibili task selection requires exactly one selection id.",
                ));
            }
            Ok(BilibiliPlaybackSelectionPlan {
                task_selection: Some(selection.clone()),
                mode: BilibiliPlaybackSelectionPlanMode::ExplicitIds {
                    selection_ids: selection.selection_ids,
                },
            })
        }
        BILIBILI_TASK_SELECTION_MODE_MULTIPLE => {
            require_no_range(&selection)?;
            require_selection_ids(&selection)?;
            Ok(BilibiliPlaybackSelectionPlan {
                task_selection: Some(selection.clone()),
                mode: BilibiliPlaybackSelectionPlanMode::ExplicitIds {
                    selection_ids: selection.selection_ids,
                },
            })
        }
        BILIBILI_TASK_SELECTION_MODE_RANGE => {
            if !selection.selection_ids.is_empty() {
                return Err(Status::invalid_argument(
                    "Range Bilibili task selection cannot include explicit ids.",
                ));
            }
            if selection.range_start_index == 0 || selection.range_end_index == 0 {
                return Err(Status::invalid_argument(
                    "Range Bilibili task selection requires 1-based start and end indexes.",
                ));
            }
            if selection.range_start_index > selection.range_end_index {
                return Err(Status::invalid_argument(
                    "Range Bilibili task selection start index cannot exceed end index.",
                ));
            }
            Ok(BilibiliPlaybackSelectionPlan {
                task_selection: Some(selection.clone()),
                mode: BilibiliPlaybackSelectionPlanMode::ExplicitRange {
                    start_index: selection.range_start_index,
                    end_index: selection.range_end_index,
                },
            })
        }
        BILIBILI_TASK_SELECTION_MODE_ALL => {
            if has_payload {
                return Err(Status::invalid_argument(
                    "All Bilibili task selection cannot include ids or a range.",
                ));
            }
            Ok(BilibiliPlaybackSelectionPlan {
                task_selection: Some(selection),
                mode: BilibiliPlaybackSelectionPlanMode::ExplicitAll,
            })
        }
        _ => Err(Status::invalid_argument(
            "Unknown Bilibili task selection mode.",
        )),
    }
}

fn normalized_bilibili_task_selection(
    mut selection: BilibiliTaskSelection,
) -> BilibiliTaskSelection {
    let mut seen_selection_ids = HashSet::new();
    let selection_ids = selection
        .selection_ids
        .into_iter()
        .map(|selection_id| selection_id.trim().to_owned())
        .filter(|selection_id| !selection_id.is_empty())
        .filter(|selection_id| seen_selection_ids.insert(selection_id.clone()))
        .collect::<Vec<_>>();
    selection.selection_ids = selection_ids;
    selection
}

fn selection_is_default_empty(selection: &BilibiliTaskSelection) -> bool {
    matches!(
        selection.mode,
        BILIBILI_TASK_SELECTION_MODE_UNSPECIFIED | BILIBILI_TASK_SELECTION_MODE_DEFAULT
    ) && selection.selection_ids.is_empty()
        && selection.range_start_index == 0
        && selection.range_end_index == 0
}

fn require_no_range(selection: &BilibiliTaskSelection) -> Result<(), Status> {
    if selection.range_start_index != 0 || selection.range_end_index != 0 {
        return Err(Status::invalid_argument(
            "Explicit id Bilibili task selection cannot include a range.",
        ));
    }
    Ok(())
}

fn require_selection_ids(selection: &BilibiliTaskSelection) -> Result<(), Status> {
    if selection.selection_ids.is_empty() {
        return Err(Status::invalid_argument(
            "Bilibili task selection requires at least one selection id.",
        ));
    }
    Ok(())
}

impl From<BilibiliInputResolution> for BilibiliResolveResult {
    fn from(resolution: BilibiliInputResolution) -> Self {
        Self {
            source: resolution.source,
            title: resolution.title,
            source_kind: resolution.source_kind,
            candidates: resolution
                .candidates
                .into_iter()
                .map(ProtoBilibiliResolvedCandidate::from)
                .collect(),
            default_selection_id: resolution.default_selection_id,
            candidates_truncated: resolution.candidates_truncated,
        }
    }
}

impl From<AdapterBilibiliResolvedCandidate> for ProtoBilibiliResolvedCandidate {
    fn from(candidate: AdapterBilibiliResolvedCandidate) -> Self {
        Self {
            selection_id: candidate.selection_id,
            title: candidate.title,
            subtitle: candidate.subtitle,
            source_kind: candidate.source_kind,
            content_id: candidate.content_id,
            index: candidate.index,
            duration_seconds: candidate.duration_seconds.unwrap_or_default().into(),
            cover_uri: candidate.cover_uri,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use crate::{
        bbdown_adapter::{
            BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest,
            BilibiliMediaRequestKind, BilibiliPlaybackAbrGroup, BilibiliPlaybackAbrGroupKind,
            BilibiliPlaybackAbrLevel, BilibiliPlaybackAbrMetadata, BilibiliPlaybackEntry,
            BilibiliPlaybackVariantSelection, BilibiliPlaybackVariantSelectionPolicy,
            BilibiliSelectedPlaybackVariant,
        },
        bilibili_playback::{
            BilibiliInputResolution, BilibiliInputResolveFuture, BilibiliInputResolveRequest,
            BilibiliPlaybackPlanner, BilibiliPlaybackPlanningFuture,
            BilibiliPlaybackPlanningRequest, BilibiliResolvedCandidate,
        },
        config::CacheServerOptions,
        generated::tvos_net_player::v1::{
            BilibiliCredentialState, BilibiliPlaybackOptions, BilibiliTaskSelection,
            CreateBilibiliPlaybackTaskRequest, DeleteLibraryItemRequest,
            GetBilibiliCredentialStatusRequest, GetLibraryItemRequest, GetPlaybackSourceRequest,
            GetServerInfoRequest, LibraryFilter, LibrarySource, ListLibraryItemsRequest,
            ResolveBilibiliInputRequest, TaskKind, TaskState,
        },
        hls_cache::sanitized_completed_session,
        hls_network_policy::HlsWeakNetworkState as RuntimeTestHlsWeakNetworkState,
        playback_policy::{
            CompatibleVariantPreference, TranscodingPreference, WeakNetworkPreference,
        },
    };
    use axum::{
        Router,
        body::{Body, Bytes},
        extract::{Path as AxumPath, State},
        http::{HeaderMap, HeaderValue, Response, StatusCode, header::CONTENT_TYPE},
        routing::get,
    };
    use tokio::sync::{mpsc, oneshot};
    use tokio_stream::StreamExt;

    use super::*;

    #[tokio::test]
    async fn get_server_info_advertises_bilibili_resolve_capability() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        let info = service
            .get_server_info(Request::new(GetServerInfoRequest {}))
            .await
            .expect("server info should succeed")
            .into_inner();

        assert!(
            info.capabilities
                .contains(&(ServerCapability::BilibiliTasks as i32))
        );
        assert!(
            info.capabilities
                .contains(&(ServerCapability::BilibiliResolve as i32))
        );
        assert!(
            info.capabilities
                .contains(&(ServerCapability::BilibiliTaskSelection as i32))
        );
        assert!(
            info.capabilities
                .contains(&(ServerCapability::BilibiliCredentialStatus as i32))
        );
        assert!(
            info.capabilities
                .contains(&(ServerCapability::BilibiliCredentialProfiles as i32))
        );
        assert!(
            info.capabilities
                .contains(&(ServerCapability::BilibiliPlaybackPolicy as i32))
        );
        assert!(
            !info
                .capabilities
                .contains(&(ServerCapability::BilibiliLoginSessions as i32))
        );
        assert!(
            !info
                .capabilities
                .contains(&(ServerCapability::LanTranscoding as i32))
        );
    }

    #[tokio::test]
    async fn get_server_info_advertises_lan_transcoding_when_enabled() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            lan_transcoding_enabled: true,
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        let info = service
            .get_server_info(Request::new(GetServerInfoRequest {}))
            .await
            .expect("server info should succeed")
            .into_inner();

        assert!(
            info.capabilities
                .contains(&(ServerCapability::LanTranscoding as i32))
        );
    }

    #[tokio::test]
    async fn get_bilibili_credential_status_reports_not_configured_without_secrets() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        let status = service
            .get_bilibili_credential_status(Request::new(GetBilibiliCredentialStatusRequest {}))
            .await
            .expect("credential status should succeed")
            .into_inner();

        assert_eq!(BilibiliCredentialState::NotConfigured, status.state());
        assert!(!status.credential_path_configured);
        assert!(!status.credential_file_loaded);
        assert!(!status.web_cookie_present);
        assert!(!status.access_key_present);
        assert!(!status.tv_access_key_present);
        assert!(status.restricted_area.is_empty());
        assert_eq!(0, status.restricted_playurl_proxy_count);
        assert_eq!(0, status.restricted_api_proxy_count);
        assert!(
            !status
                .message
                .contains(temp.path().to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn get_bilibili_credential_status_reports_loaded_material_without_secret_values() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .join("cache")
            .canonicalize()
            .unwrap_or_else(|_| temp.path().join("cache"));
        fs::create_dir_all(&root_path).expect("cache root should be created");
        let credentials_path = temp.path().join("credentials.json");
        fs::write(
            &credentials_path,
            r#"{"cookie":"SESSDATA=secret","access_key":"access-token","tv_access_key":"tv-token"}"#,
        )
        .expect("credential file should be written");
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            bbdown_credential_path: Some(credentials_path.clone()),
            bbdown_restricted_area: Some(crate::config::BbdownRestrictedArea::Hk),
            bbdown_restricted_area_proxies: vec![crate::config::BbdownRestrictedProxy {
                area: Some(crate::config::BbdownRestrictedArea::Hk),
                base_url: "https://playurl.example.test/secret/path?token=hidden".to_owned(),
            }],
            bbdown_restricted_api_proxies: vec![crate::config::BbdownRestrictedProxy {
                area: None,
                base_url: "https://api.example.test/secret/path?token=hidden".to_owned(),
            }],
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        let status = service
            .get_bilibili_credential_status(Request::new(GetBilibiliCredentialStatusRequest {}))
            .await
            .expect("credential status should succeed")
            .into_inner();

        assert_eq!(BilibiliCredentialState::Ready, status.state());
        assert!(status.credential_path_configured);
        assert!(status.credential_file_loaded);
        assert!(status.web_cookie_present);
        assert!(status.access_key_present);
        assert!(status.tv_access_key_present);
        assert_eq!("hk", status.restricted_area);
        assert_eq!(1, status.restricted_playurl_proxy_count);
        assert_eq!(1, status.restricted_api_proxy_count);
        assert!(!status.message.contains("secret"));
        assert!(!status.message.contains("token"));
        assert!(
            !status
                .message
                .contains(credentials_path.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn get_bilibili_credential_status_reports_selected_profile() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .join("cache")
            .canonicalize()
            .unwrap_or_else(|_| temp.path().join("cache"));
        fs::create_dir_all(&root_path).expect("cache root should be created");
        let credentials_path = temp.path().join("credentials.json");
        fs::write(
            &credentials_path,
            r#"{
                "version": 1,
                "default_profile": "default",
                "profiles": {
                    "default": {
                        "cookie": "SESSDATA=default"
                    },
                    "living-room": {
                        "access_key": "living-access",
                        "tv_access_key": "living-tv"
                    }
                }
            }"#,
        )
        .expect("credential file should be written");
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            bbdown_credential_path: Some(credentials_path),
            bbdown_credential_profile: Some("living-room".to_owned()),
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        let status = service
            .get_bilibili_credential_status(Request::new(GetBilibiliCredentialStatusRequest {}))
            .await
            .expect("credential status should succeed")
            .into_inner();

        assert_eq!(BilibiliCredentialState::Ready, status.state());
        assert_eq!("living-room", status.active_profile_id);
        assert_eq!("default", status.default_profile_id);
        assert_eq!(2, status.profile_count);
        assert!(!status.web_cookie_present);
        assert!(status.access_key_present);
        assert!(status.tv_access_key_present);
        assert_eq!(2, status.profiles.len());
        assert!(
            status
                .profiles
                .iter()
                .any(|profile| profile.id == "living-room"
                    && profile.is_active
                    && !profile.is_default
                    && profile.access_key_present
                    && profile.tv_access_key_present
                    && !profile.web_cookie_present)
        );
        assert!(!status.message.contains("living-access"));
        assert!(!status.message.contains("living-tv"));
    }

    #[tokio::test]
    async fn list_bilibili_credential_profiles_reports_redacted_profiles() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .join("cache")
            .canonicalize()
            .unwrap_or_else(|_| temp.path().join("cache"));
        fs::create_dir_all(&root_path).expect("cache root should be created");
        let credentials_path = temp.path().join("credentials.json");
        fs::write(
            &credentials_path,
            r#"{
                "version": 1,
                "default_profile": "default",
                "profiles": {
                    "default": {
                        "cookie": "SESSDATA=default"
                    },
                    "living-room": {
                        "access_key": "living-access"
                    }
                }
            }"#,
        )
        .expect("credential file should be written");
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            bbdown_credential_path: Some(credentials_path),
            bbdown_credential_profile: Some("living-room".to_owned()),
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        let profiles = service
            .list_bilibili_credential_profiles(Request::new(
                ListBilibiliCredentialProfilesRequest {},
            ))
            .await
            .expect("profile list should succeed")
            .into_inner();

        assert_eq!("living-room", profiles.active_profile_id);
        assert_eq!("default", profiles.default_profile_id);
        assert_eq!(2, profiles.profiles.len());
        assert!(
            profiles
                .profiles
                .iter()
                .any(|profile| profile.id == "default"
                    && profile.is_default
                    && !profile.is_active
                    && profile.web_cookie_present)
        );
        assert!(
            profiles
                .profiles
                .iter()
                .any(|profile| profile.id == "living-room"
                    && profile.is_active
                    && !profile.is_default
                    && profile.access_key_present)
        );
    }

    #[tokio::test]
    async fn bilibili_login_session_foundation_shares_unsupported_session_across_services() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let credentials_path = temp.path().join("credentials.json");
        fs::write(
            &credentials_path,
            r#"{
                "version": 1,
                "default_profile": "living-room",
                "profiles": {
                    "living-room": {
                        "cookie": "SESSDATA=living-room"
                    }
                }
            }"#,
        )
        .expect("credential file should be written");
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            bbdown_credential_path: Some(credentials_path),
            ..CacheServerOptions::default()
        });
        let creator = ServerGrpcService::new(state.clone());
        let reader = ServerGrpcService::new(state);

        let session = creator
            .start_bilibili_login_session(Request::new(StartBilibiliLoginSessionRequest {
                profile_id: String::new(),
                method: BilibiliLoginMethod::WebQr.into(),
            }))
            .await
            .expect("login session start should succeed")
            .into_inner();

        assert!(!session.id.is_empty());
        assert_eq!("living-room", session.profile_id);
        assert_eq!(BilibiliLoginMethod::WebQr, session.method());
        assert_eq!(BilibiliLoginSessionState::Unsupported, session.state());
        assert!(session.verification_uri.is_empty());
        assert!(!session.message.contains("cookie"));

        let fetched = reader
            .get_bilibili_login_session(Request::new(GetBilibiliLoginSessionRequest {
                session_id: session.id.clone(),
            }))
            .await
            .expect("login session get should succeed")
            .into_inner();

        assert_eq!(session, fetched);
    }

    #[tokio::test]
    async fn bilibili_login_session_rejects_unknown_method() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        let error = service
            .start_bilibili_login_session(Request::new(StartBilibiliLoginSessionRequest {
                profile_id: String::new(),
                method: i32::MAX,
            }))
            .await
            .expect_err("unknown login method should be rejected");

        assert_eq!(tonic::Code::InvalidArgument, error.code());
    }

    #[tokio::test]
    async fn bilibili_login_session_rejects_oversized_profile_without_mutating_store() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        service
            .start_bilibili_login_session(Request::new(StartBilibiliLoginSessionRequest {
                profile_id: "a".repeat(MAX_BILIBILI_LOGIN_PROFILE_ID_BYTES),
                method: BilibiliLoginMethod::WebQr.into(),
            }))
            .await
            .expect("maximum-length profile ID should be accepted");
        let sessions_before = service
            .login_sessions
            .lock()
            .expect("session store should be available")
            .clone();

        let error = service
            .start_bilibili_login_session(Request::new(StartBilibiliLoginSessionRequest {
                profile_id: "a".repeat(MAX_BILIBILI_LOGIN_PROFILE_ID_BYTES + 1),
                method: BilibiliLoginMethod::WebQr.into(),
            }))
            .await
            .expect_err("oversized profile ID should be rejected");

        assert_eq!(tonic::Code::InvalidArgument, error.code());
        assert_eq!(
            sessions_before,
            *service
                .login_sessions
                .lock()
                .expect("session store should be available")
        );
    }

    #[tokio::test]
    async fn bilibili_login_session_store_evicts_oldest_session_at_capacity() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);
        let mut oldest_session_id = String::new();

        for index in 0..=MAX_BILIBILI_LOGIN_SESSIONS {
            let session = service
                .start_bilibili_login_session(Request::new(StartBilibiliLoginSessionRequest {
                    profile_id: format!("profile-{index}"),
                    method: BilibiliLoginMethod::WebQr.into(),
                }))
                .await
                .expect("login session start should succeed")
                .into_inner();
            if index == 0 {
                oldest_session_id = session.id;
            }
        }

        assert_eq!(
            MAX_BILIBILI_LOGIN_SESSIONS,
            service
                .login_sessions
                .lock()
                .expect("session store should be available")
                .len()
        );
        let error = service
            .get_bilibili_login_session(Request::new(GetBilibiliLoginSessionRequest {
                session_id: oldest_session_id,
            }))
            .await
            .expect_err("oldest session should be evicted");
        assert_eq!(tonic::Code::NotFound, error.code());
    }

    #[tokio::test]
    async fn get_bilibili_credential_status_reports_runtime_load_error_without_path() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .join("cache")
            .canonicalize()
            .unwrap_or_else(|_| temp.path().join("cache"));
        fs::create_dir_all(&root_path).expect("cache root should be created");
        let credentials_path = temp.path().join("missing-credentials.json");
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            bbdown_credential_path: Some(credentials_path.clone()),
            ..CacheServerOptions::default()
        });
        let service = ServerGrpcService::new(state);

        let status = service
            .get_bilibili_credential_status(Request::new(GetBilibiliCredentialStatusRequest {}))
            .await
            .expect("credential status should succeed")
            .into_inner();

        assert_eq!(BilibiliCredentialState::Error, status.state());
        assert!(status.credential_path_configured);
        assert!(!status.credential_file_loaded);
        assert!(
            !status
                .message
                .contains(credentials_path.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn delete_library_item_requires_explicit_enablement() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        fs::write(root_path.join("sample.mp4"), b"sample").expect("media file should be written");
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let service = CacheGrpcService::new(state);

        let error = service
            .delete_library_item(Request::new(DeleteLibraryItemRequest {
                id: "local.default.c2FtcGxlLm1wNA".to_owned(),
            }))
            .await
            .expect_err("delete should require explicit enablement");

        assert_eq!(tonic::Code::PermissionDenied, error.code());
        assert!(root_path.join("sample.mp4").exists());
    }

    #[tokio::test]
    async fn resolve_bilibili_input_returns_selectable_candidates() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let planner = StaticResolvePlanner {
            requests: Arc::clone(&requests),
            resolution: BilibiliInputResolution {
                source: "BV1multi".to_owned(),
                title: "Multi page video".to_owned(),
                source_kind: "video".to_owned(),
                default_selection_id: String::new(),
                candidates_truncated: true,
                candidates: vec![
                    BilibiliResolvedCandidate {
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: "Page 1".to_owned(),
                        source_kind: "video_page".to_owned(),
                        content_id: "1001".to_owned(),
                        index: 1,
                        duration_seconds: Some(60),
                        cover_uri: "https://example.test/cover.jpg".to_owned(),
                    },
                    BilibiliResolvedCandidate {
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: "Page 2".to_owned(),
                        source_kind: "video_page".to_owned(),
                        content_id: "1002".to_owned(),
                        index: 2,
                        duration_seconds: Some(75),
                        cover_uri: "https://example.test/cover.jpg".to_owned(),
                    },
                ],
            },
        };
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(planner),
        );
        let service = TaskGrpcService::new(state);

        let resolved = service
            .resolve_bilibili_input(Request::new(ResolveBilibiliInputRequest {
                url_or_id: "  BV1multi  ".to_owned(),
                options: Some(BilibiliPlaybackOptions {
                    quality_preference: "1080p".to_owned(),
                    encoding_preference: "h264".to_owned(),
                    prefer_tv_api: false,
                    audio_language: "ja-jp".to_owned(),
                    playback_policy: None,
                }),
            }))
            .await
            .expect("resolve should succeed")
            .into_inner();

        assert_eq!("Multi page video", resolved.title);
        assert_eq!("video", resolved.source_kind);
        assert_eq!("", resolved.default_selection_id);
        assert!(resolved.candidates_truncated);
        assert_eq!(2, resolved.candidates.len());
        assert_eq!("page:2", resolved.candidates[1].selection_id);
        assert_eq!("Part 2", resolved.candidates[1].title);
        assert_eq!(75, resolved.candidates[1].duration_seconds);

        let requests = requests.lock().expect("request log should not be poisoned");
        assert_eq!(1, requests.len());
        assert_eq!("BV1multi", requests[0].0);
        assert_eq!(
            Some("1080p"),
            requests[0]
                .1
                .as_ref()
                .map(|options| options.quality_preference.as_str())
        );
    }

    #[tokio::test]
    async fn resolve_bilibili_input_waits_for_playback_planning_permit() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let planner = StaticResolvePlanner {
            requests: Arc::clone(&requests),
            resolution: BilibiliInputResolution {
                source: "BV1multi".to_owned(),
                title: "Multi page video".to_owned(),
                source_kind: "video".to_owned(),
                default_selection_id: "page:1".to_owned(),
                candidates_truncated: false,
                candidates: vec![BilibiliResolvedCandidate {
                    selection_id: "page:1".to_owned(),
                    title: "Part 1".to_owned(),
                    subtitle: "Page 1".to_owned(),
                    source_kind: "video_page".to_owned(),
                    content_id: "1001".to_owned(),
                    index: 1,
                    duration_seconds: Some(60),
                    cover_uri: "https://example.test/cover.jpg".to_owned(),
                }],
            },
        };
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(planner),
        );
        let held_permit = Arc::clone(&state.playback_planning_permits)
            .acquire_owned()
            .await
            .expect("playback planning permit should be acquired");
        let service = TaskGrpcService::new(state);

        let pending_resolve = tokio::spawn(async move {
            service
                .resolve_bilibili_input(Request::new(ResolveBilibiliInputRequest {
                    url_or_id: "BV1multi".to_owned(),
                    options: None,
                }))
                .await
        });
        sleep(Duration::from_millis(100)).await;
        assert!(!pending_resolve.is_finished());
        assert!(
            requests
                .lock()
                .expect("request log should not be poisoned")
                .is_empty()
        );

        drop(held_permit);
        let resolved = tokio::time::timeout(Duration::from_secs(1), pending_resolve)
            .await
            .expect("resolve should finish after the playback planning permit is released")
            .expect("resolve task should not panic")
            .expect("resolve should succeed")
            .into_inner();

        assert_eq!("page:1", resolved.default_selection_id);
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_passes_selection_id_to_planner() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(RecordingPlaybackPlanner {
                requests: Arc::clone(&requests),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1select".to_owned(),
                options: None,
                selection_id: "page:2".to_owned(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_DEFAULT,
                    selection_ids: Vec::new(),
                    range_start_index: 0,
                    range_end_index: 0,
                }),
            }))
            .await
            .expect("playback task should be created")
            .into_inner();

        let playable = wait_for_task_state(&tasks, &created.id, TaskState::Playable).await;
        assert_eq!(TaskState::Playable, playable.state());
        let requests = requests.lock().expect("request log should not be poisoned");
        assert_eq!(
            vec![("BV1select".to_owned(), Some("page:2".to_owned()))],
            *requests
        );
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_rejects_invalid_selection_range() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(RecordingPlaybackPlanner {
                requests: Arc::clone(&requests),
            }),
        );
        let service = TaskGrpcService::new(state);

        let error = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1range".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 3,
                    range_end_index: 2,
                }),
            }))
            .await
            .expect_err("invalid range should be rejected");

        assert_eq!(tonic::Code::InvalidArgument, error.code());
        assert!(error.message().contains("start index"));
        let requests = requests.lock().expect("request log should not be poisoned");
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_executes_range_selection_results() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let resolve_requests = Arc::new(Mutex::new(Vec::new()));
        let playback_requests = Arc::new(Mutex::new(Vec::new()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndRecordingPlaybackPlanner {
                resolve_requests: Arc::clone(&resolve_requests),
                playback_requests: Arc::clone(&playback_requests),
                resolution: sample_resolution_with_pages(),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1range".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 1,
                    range_end_index: 2,
                }),
            }))
            .await
            .expect("range task should be created")
            .into_inner();

        let playable = wait_for_task_state(&tasks, &created.id, TaskState::Playable).await;

        assert_eq!(TaskState::Playable, playable.state());
        assert_eq!(0.0, playable.progress);
        assert_eq!(
            Some(BILIBILI_TASK_SELECTION_MODE_RANGE),
            playable
                .bilibili_selection
                .as_ref()
                .map(|selection| selection.mode)
        );
        assert_eq!(2, playable.result_items.len());
        assert_eq!(created.id, playable.result_items[0].id);
        assert_eq!(
            format!("{}-result-2", created.id),
            playable.result_items[1].id
        );
        assert_eq!(
            vec!["page:1".to_owned(), "page:2".to_owned()],
            playable
                .result_items
                .iter()
                .map(|item| item.selection_id.clone())
                .collect::<Vec<_>>()
        );
        assert!(playable.result_items.iter().all(|item| {
            item.state == i32::from(TaskState::Playable)
                && item.playback_source.is_some()
                && item.playback_session.is_some()
        }));
        assert_eq!(
            vec![("BV1range".to_owned(), None)],
            *resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
        );
        assert_eq!(
            vec![
                ("BV1range".to_owned(), Some("page:1".to_owned())),
                ("BV1range".to_owned(), Some("page:2".to_owned())),
            ],
            *playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
        );
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_executes_all_selection_results() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let resolve_requests = Arc::new(Mutex::new(Vec::new()));
        let playback_requests = Arc::new(Mutex::new(Vec::new()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndRecordingPlaybackPlanner {
                resolve_requests: Arc::clone(&resolve_requests),
                playback_requests: Arc::clone(&playback_requests),
                resolution: sample_resolution_with_pages(),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1all".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_ALL,
                    selection_ids: Vec::new(),
                    range_start_index: 0,
                    range_end_index: 0,
                }),
            }))
            .await
            .expect("all-selection task should be created")
            .into_inner();

        let playable = wait_for_task_state(&tasks, &created.id, TaskState::Playable).await;

        assert_eq!(TaskState::Playable, playable.state());
        assert_eq!(
            Some(BILIBILI_TASK_SELECTION_MODE_ALL),
            playable
                .bilibili_selection
                .as_ref()
                .map(|selection| selection.mode)
        );
        assert_eq!(3, playable.result_items.len());
        assert_eq!(created.id, playable.result_items[0].id);
        assert_eq!(
            format!("{}-result-2", created.id),
            playable.result_items[1].id
        );
        assert_eq!(
            format!("{}-result-3", created.id),
            playable.result_items[2].id
        );
        assert_eq!(
            vec![
                "page:1".to_owned(),
                "page:2".to_owned(),
                "page:3".to_owned(),
            ],
            playable
                .result_items
                .iter()
                .map(|item| item.selection_id.clone())
                .collect::<Vec<_>>()
        );
        assert!(playable.result_items.iter().all(|item| {
            item.state == i32::from(TaskState::Playable)
                && item.playback_source.is_some()
                && item.playback_session.is_some()
        }));
        assert_eq!(
            vec![("BV1all".to_owned(), None)],
            *resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
        );
        assert_eq!(
            vec![
                ("BV1all".to_owned(), Some("page:1".to_owned())),
                ("BV1all".to_owned(), Some("page:2".to_owned())),
                ("BV1all".to_owned(), Some("page:3".to_owned())),
            ],
            *playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
        );
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_fails_partial_range_resolution() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let resolve_requests = Arc::new(Mutex::new(Vec::new()));
        let playback_requests = Arc::new(Mutex::new(Vec::new()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndRecordingPlaybackPlanner {
                resolve_requests: Arc::clone(&resolve_requests),
                playback_requests: Arc::clone(&playback_requests),
                resolution: sample_resolution_with_pages(),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1partial-range".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 2,
                    range_end_index: 4,
                }),
            }))
            .await
            .expect("partial range task should be accepted before async resolution")
            .into_inner();

        let failed = wait_for_task_state(&tasks, &created.id, TaskState::Failed).await;

        assert!(failed.message.contains("could not be fully resolved"));
        assert_eq!(
            vec![("BV1partial-range".to_owned(), None)],
            *resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
        );
        assert!(
            playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn credential_configured_rpc_errors_omit_upstream_detail() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let credential_path = root_path.join("credentials.json");
        fs::write(&credential_path, "{}").expect("empty credential store should be written");
        let sensitive_detail = "upstream failed at https://example.test/playurl?access_key=credential-sensitive-marker";
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                bbdown_credential_path: Some(credential_path),
                ..CacheServerOptions::default()
            },
            Arc::new(FailingPlaybackPlanner {
                detail: sensitive_detail.to_owned(),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1credential-error".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
            }))
            .await
            .expect("playback task should be accepted before async planning")
            .into_inner();
        let _ = wait_for_task_state(&tasks, &created.id, TaskState::Failed).await;

        let snapshot = service
            .get_task(Request::new(GetTaskRequest {
                id: created.id.clone(),
            }))
            .await
            .expect("failed task should remain readable")
            .into_inner();
        assert_eq!(
            crate::credential_safe_client_error(true, &sensitive_detail),
            snapshot.message
        );
        assert!(!snapshot.message.contains("credential-sensitive-marker"));

        let legacy_task = tasks
            .create_bilibili_task("BV1stored-credential-error", None)
            .expect("legacy task should be created");
        tasks
            .complete_task_failed(&legacy_task.id, sensitive_detail.to_owned())
            .expect("legacy task should store its original failure");
        let legacy_snapshot = service
            .get_task(Request::new(GetTaskRequest { id: legacy_task.id }))
            .await
            .expect("legacy failed task should remain readable")
            .into_inner();
        assert_eq!(
            crate::credential_safe_client_error(true, &sensitive_detail),
            legacy_snapshot.message
        );
        assert!(
            !legacy_snapshot
                .message
                .contains("credential-sensitive-marker")
        );

        let status = service
            .resolve_bilibili_input(Request::new(ResolveBilibiliInputRequest {
                url_or_id: "BV1credential-error".to_owned(),
                options: None,
            }))
            .await
            .expect_err("resolve should return the planner error");
        assert_eq!(tonic::Code::FailedPrecondition, status.code());
        assert_eq!(
            crate::credential_safe_client_error(true, &sensitive_detail),
            status.message()
        );
        assert!(!status.message().contains("credential-sensitive-marker"));
    }

    #[tokio::test]
    async fn credential_configured_rpc_omits_running_download_detail_from_get_and_watch() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let credential_path = root_path.join("credentials.json");
        fs::write(&credential_path, "{}").expect("empty credential store should be written");
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            bbdown_credential_path: Some(credential_path),
            ..CacheServerOptions::default()
        });
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);
        let task = tasks
            .create_bilibili_task("BV1running-credential-error", None)
            .expect("download task should be created");
        let _work_item = tasks
            .try_claim_next_bilibili_task()
            .expect("download task should become running");
        assert!(
            tasks.update_task_progress(
                &task.id,
                BilibiliTaskProgress {
                    progress: Some(0.25),
                    message: Some(
                        "retrying https://example.test/media?access_key=running-sensitive-marker"
                            .to_owned(),
                    ),
                    ..Default::default()
                },
            )
        );

        let snapshot = service
            .get_task(Request::new(GetTaskRequest {
                id: task.id.clone(),
            }))
            .await
            .expect("running task should remain readable")
            .into_inner();
        assert_eq!(TaskState::Running, snapshot.state());
        assert_eq!(0.25, snapshot.progress);
        assert_eq!(
            crate::CREDENTIAL_SAFE_CLIENT_RUNNING_DETAIL,
            snapshot.message
        );
        assert!(!snapshot.message.contains("running-sensitive-marker"));

        let mut stream = service
            .watch_tasks(Request::new(WatchTasksRequest {
                ids: vec![task.id.clone()],
            }))
            .await
            .expect("running task watch should start")
            .into_inner();
        let watched_snapshot = stream
            .next()
            .await
            .expect("watch should include its initial snapshot")
            .expect("initial task event should succeed")
            .task
            .expect("initial event should include a task");
        assert_eq!(
            crate::CREDENTIAL_SAFE_CLIENT_RUNNING_DETAIL,
            watched_snapshot.message
        );

        assert!(tasks.update_task_progress(
            &task.id,
            BilibiliTaskProgress {
                progress: Some(0.5),
                message: Some("BBDown failed with watch-sensitive-marker".to_owned()),
                ..Default::default()
            },
        ));
        let watched_update = stream
            .next()
            .await
            .expect("watch should include the progress update")
            .expect("progress task event should succeed")
            .task
            .expect("progress event should include a task");
        assert_eq!(0.5, watched_update.progress);
        assert_eq!(
            crate::CREDENTIAL_SAFE_CLIENT_RUNNING_DETAIL,
            watched_update.message
        );
        assert!(!watched_update.message.contains("watch-sensitive-marker"));
    }

    #[tokio::test]
    async fn credential_configured_rpc_omits_persisted_playable_cache_fill_detail() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let credential_path = root_path.join("credentials.json");
        let task_state_path = root_path.join(".state").join("tasks.json");
        fs::write(&credential_path, "{}").expect("empty credential store should be written");
        let state = AppState::new(CacheServerOptions {
            root_path,
            task_state_path: task_state_path.clone(),
            bilibili_worker_enabled: false,
            bbdown_credential_path: Some(credential_path),
            ..CacheServerOptions::default()
        });
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);
        let created = tasks
            .create_bilibili_playback_task("BV1playable-credential-error", None, None)
            .expect("playback task should be created");
        tasks
            .complete_playback_playable(
                &created.task.id,
                "Playable".to_owned(),
                PlaybackSource {
                    item_id: created.task.id.clone(),
                    variant_id: "source".to_owned(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: format!(
                        "http://media.example.test:8080/hls/{}/master.m3u8",
                        created.task.id
                    ),
                    expires_at: None,
                },
                BilibiliPlaybackSession {
                    id: created.task.id.clone(),
                    selected_variant_id: "source".to_owned(),
                    ..Default::default()
                },
            )
            .expect("playback task should become playable");
        // Synthetic token fixture: joey-private-v3/access-a.
        let synthetic_access_token = "codex_synth_v1_access_a";
        let sensitive_detail = format!(
            "Playable online; offline cache fill failed: upstream request https://example.test/media?access_key={synthetic_access_token}"
        );
        let degraded = tasks
            .fail_hls_cache_fill_for_playback_session(
                &created.task.id,
                &created.task.id,
                sensitive_detail.clone(),
            )
            .expect("cache fill failure should be accepted")
            .expect("cache fill failure should update the playable task");
        assert_eq!(TaskState::Playable, degraded.state());
        assert!(
            fs::read_to_string(&task_state_path)
                .expect("persisted task state should remain readable")
                .contains(synthetic_access_token)
        );

        let snapshot = service
            .get_task(Request::new(GetTaskRequest {
                id: created.task.id.clone(),
            }))
            .await
            .expect("persisted playable task should remain readable")
            .into_inner();
        assert_eq!(TaskState::Playable, snapshot.state());
        assert_eq!(
            crate::credential_safe_client_error(true, &sensitive_detail),
            snapshot.message
        );
        assert!(!snapshot.message.contains(synthetic_access_token));

        let mut stream = service
            .watch_tasks(Request::new(WatchTasksRequest {
                ids: vec![created.task.id],
            }))
            .await
            .expect("playable task watch should start")
            .into_inner();
        let watched_snapshot = stream
            .next()
            .await
            .expect("watch should include its initial snapshot")
            .expect("initial task event should succeed")
            .task
            .expect("initial event should include a task");
        assert_eq!(TaskState::Playable, watched_snapshot.state());
        assert_eq!(snapshot.message, watched_snapshot.message);
        assert!(!watched_snapshot.message.contains(synthetic_access_token));
    }

    #[tokio::test]
    async fn credential_configured_rpc_preserves_internal_cancellation_messages() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let credential_path = root_path.join("credentials.json");
        fs::write(&credential_path, "{}").expect("empty credential store should be written");
        let state = AppState::new(CacheServerOptions {
            root_path,
            bilibili_worker_enabled: false,
            bbdown_credential_path: Some(credential_path),
            ..CacheServerOptions::default()
        });
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let queued = tasks
            .create_bilibili_task("BV1queued-cancel", None)
            .expect("queued task should be created");
        let queued_cancelled = service
            .cancel_task(Request::new(CancelTaskRequest { id: queued.id }))
            .await
            .expect("queued task cancellation should succeed")
            .into_inner();
        assert_eq!(TaskState::Cancelled, queued_cancelled.state());
        assert_eq!(
            "Cancelled before the download adapter started.",
            queued_cancelled.message
        );
        assert!(!queued_cancelled.message.contains("server_bug"));

        let running = tasks
            .create_bilibili_task("BV1running-cancel", None)
            .expect("running task should be created");
        let work_item = tasks
            .try_claim_next_bilibili_task()
            .expect("download task should become running");
        assert_eq!(running.id, work_item.task_id);
        service
            .cancel_task(Request::new(CancelTaskRequest {
                id: running.id.clone(),
            }))
            .await
            .expect("running task cancellation should be requested");
        tasks
            .complete_task_cancelled(&running.id, "Cancelled by request.".to_owned())
            .expect("running task should finish cancellation");

        let running_cancelled = service
            .get_task(Request::new(GetTaskRequest { id: running.id }))
            .await
            .expect("cancelled task should remain readable")
            .into_inner();
        assert_eq!(TaskState::Cancelled, running_cancelled.state());
        assert_eq!("Cancelled by request.", running_cancelled.message);
        assert!(!running_cancelled.message.contains("server_bug"));

        let planning = tasks
            .create_bilibili_playback_task("BV1planning-cancel", None, None)
            .expect("playback planning task should be created");
        tasks
            .complete_task_cancelled(
                &planning.task.id,
                PLAYBACK_PLANNING_CANCELLED_MESSAGE.to_owned(),
            )
            .expect("playback planning task should finish cancellation");
        let planning_snapshot = service
            .get_task(Request::new(GetTaskRequest {
                id: planning.task.id.clone(),
            }))
            .await
            .expect("cancelled playback planning task should remain readable")
            .into_inner();
        assert_eq!(
            PLAYBACK_PLANNING_CANCELLED_MESSAGE,
            planning_snapshot.message
        );
        assert!(!planning_snapshot.message.contains("server_bug"));
        let mut planning_stream = service
            .watch_tasks(Request::new(WatchTasksRequest {
                ids: vec![planning.task.id],
            }))
            .await
            .expect("cancelled playback planning watch should start")
            .into_inner();
        let watched_planning = planning_stream
            .next()
            .await
            .expect("watch should include its initial planning snapshot")
            .expect("initial planning event should succeed")
            .task
            .expect("initial planning event should include a task");
        assert_eq!(planning_snapshot.message, watched_planning.message);

        let explicit = tasks
            .create_bilibili_playback_task("BV1result-planning-cancel", None, None)
            .expect("explicit playback task should be created");
        tasks
            .update_playback_results(
                &explicit.task.id,
                None,
                PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE.to_owned(),
                0.5,
                vec![BilibiliTaskResultItem {
                    id: format!("{}-result-1", explicit.task.id),
                    state: TaskState::Cancelled.into(),
                    message: PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE.to_owned(),
                    ..Default::default()
                }],
            )
            .expect("explicit playback result should record cancellation");
        tasks
            .complete_task_cancelled(
                &explicit.task.id,
                PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE.to_owned(),
            )
            .expect("explicit playback task should finish cancellation");
        let explicit_snapshot = service
            .get_task(Request::new(GetTaskRequest {
                id: explicit.task.id.clone(),
            }))
            .await
            .expect("cancelled explicit playback task should remain readable")
            .into_inner();
        assert_eq!(
            PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE,
            explicit_snapshot.message
        );
        assert_eq!(1, explicit_snapshot.result_items.len());
        assert_eq!(
            PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE,
            explicit_snapshot.result_items[0].message
        );
        assert!(!explicit_snapshot.message.contains("server_bug"));
        let mut explicit_stream = service
            .watch_tasks(Request::new(WatchTasksRequest {
                ids: vec![explicit.task.id],
            }))
            .await
            .expect("cancelled explicit playback watch should start")
            .into_inner();
        let watched_explicit = explicit_stream
            .next()
            .await
            .expect("watch should include its initial explicit snapshot")
            .expect("initial explicit event should succeed")
            .task
            .expect("initial explicit event should include a task");
        assert_eq!(explicit_snapshot.message, watched_explicit.message);
        assert_eq!(
            explicit_snapshot.result_items[0].message,
            watched_explicit.result_items[0].message
        );
    }

    #[tokio::test]
    async fn credential_configured_result_item_omits_upstream_detail() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let credential_path = root_path.join("credentials.json");
        fs::write(&credential_path, "{}").expect("empty credential store should be written");
        let sensitive_detail = "restricted proxy rejected playurl at https://example.test/playurl?access_key=result-sensitive-marker";
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                bbdown_credential_path: Some(credential_path),
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndScriptedPlaybackPlanner {
                resolve_requests: Arc::new(Mutex::new(Vec::new())),
                playback_requests: Arc::new(Mutex::new(Vec::new())),
                resolution: sample_resolution_with_pages(),
                results: Mutex::new(HashMap::from([(
                    "page:1".to_owned(),
                    Err(BilibiliDownloadError::Failed(sensitive_detail.to_owned())),
                )])),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1credential-result-error".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_SINGLE,
                    selection_ids: vec!["page:1".to_owned()],
                    range_start_index: 0,
                    range_end_index: 0,
                }),
            }))
            .await
            .expect("playback task should be accepted before async planning")
            .into_inner();
        let _ = wait_for_task_state(&tasks, &created.id, TaskState::Failed).await;

        let snapshot = service
            .get_task(Request::new(GetTaskRequest { id: created.id }))
            .await
            .expect("failed task should remain readable")
            .into_inner();
        assert_eq!(1, snapshot.result_items.len());
        assert_eq!(
            crate::credential_safe_client_error(true, &sensitive_detail),
            snapshot.result_items[0].message
        );
        assert!(
            snapshot.result_items[0]
                .message
                .contains("[bilibili_failure_class=restricted_proxy]")
        );
        assert!(
            !snapshot.result_items[0]
                .message
                .contains("result-sensitive-marker")
        );
    }

    #[test]
    fn task_client_boundary_redacts_failed_child_of_completed_parent() {
        let task = Task {
            state: TaskState::Completed.into(),
            message: "Completed with one playable result.".to_owned(),
            result_items: vec![BilibiliTaskResultItem {
                state: TaskState::Failed.into(),
                message: "upstream result-sensitive-marker".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let sanitized = task_for_client(task, true);

        assert_eq!("Completed with one playable result.", sanitized.message);
        assert_eq!(
            crate::credential_safe_client_error(true, &"upstream result-sensitive-marker"),
            sanitized.result_items[0].message
        );
        assert!(
            !sanitized.result_items[0]
                .message
                .contains("result-sensitive-marker")
        );
    }

    #[test]
    fn task_client_boundary_redacts_cache_fill_failure_from_playable_parent_states() {
        let sensitive_detail =
            "Playable online; offline cache fill failed: upstream response-sensitive-marker";

        for state in [TaskState::Playable, TaskState::Completed] {
            let task = Task {
                kind: TaskKind::BilibiliProgressivePlayback.into(),
                state: state.into(),
                message: sensitive_detail.to_owned(),
                ..Default::default()
            };

            let sanitized = task_for_client(task, true);

            assert_eq!(state, sanitized.state());
            assert_eq!(
                crate::credential_safe_client_error(true, &sensitive_detail),
                sanitized.message
            );
            assert!(!sanitized.message.contains("response-sensitive-marker"));
        }

        let wrapped = format!(
            "{} [bilibili_failure_class=restricted_proxy] Playable online; offline cache fill failed.",
            crate::CREDENTIAL_SAFE_CLIENT_DETAIL
        );
        let sanitized = task_for_client(
            Task {
                kind: TaskKind::BilibiliProgressivePlayback.into(),
                state: TaskState::Playable.into(),
                message: wrapped,
                ..Default::default()
            },
            true,
        );
        assert_eq!(
            format!(
                "{} [bilibili_failure_class=restricted_proxy]",
                crate::CREDENTIAL_SAFE_CLIENT_DETAIL
            ),
            sanitized.message
        );
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_fails_all_selection_for_truncated_resolution() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let resolve_requests = Arc::new(Mutex::new(Vec::new()));
        let playback_requests = Arc::new(Mutex::new(Vec::new()));
        let mut resolution = sample_resolution_with_pages();
        resolution.candidates_truncated = true;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndRecordingPlaybackPlanner {
                resolve_requests: Arc::clone(&resolve_requests),
                playback_requests: Arc::clone(&playback_requests),
                resolution,
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1truncated-all".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_ALL,
                    selection_ids: Vec::new(),
                    range_start_index: 0,
                    range_end_index: 0,
                }),
            }))
            .await
            .expect("all task should be accepted before async resolution")
            .into_inner();

        let failed = wait_for_task_state(&tasks, &created.id, TaskState::Failed).await;

        assert!(failed.message.contains("truncated"));
        assert_eq!(
            vec![("BV1truncated-all".to_owned(), None)],
            *resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
        );
        assert!(
            playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_cleans_planned_result_sessions_on_cancel() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let resolve_requests = Arc::new(Mutex::new(Vec::new()));
        let playback_requests = Arc::new(Mutex::new(Vec::new()));
        let (first_planned_sender, first_planned) = oneshot::channel();
        let (second_started_sender, second_started) = oneshot::channel();
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(CancelDuringSecondSelectionPlaybackPlanner {
                resolve_requests: Arc::clone(&resolve_requests),
                playback_requests: Arc::clone(&playback_requests),
                resolution: sample_resolution_with_pages(),
                first_planned: Mutex::new(Some(first_planned_sender)),
                second_started: Mutex::new(Some(second_started_sender)),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state.clone());

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1range-cancel".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 1,
                    range_end_index: 2,
                }),
            }))
            .await
            .expect("range task should be created")
            .into_inner();

        first_planned
            .await
            .expect("first selected result should be planned");
        second_started
            .await
            .expect("second selected result should begin planning");
        let cancel_response = service
            .cancel_task(Request::new(CancelTaskRequest {
                id: created.id.clone(),
            }))
            .await
            .expect("explicit selection task should accept cancellation")
            .into_inner();
        assert!(matches!(
            cancel_response.state(),
            TaskState::CancelRequested | TaskState::Cancelled
        ));

        let cancelled = wait_for_task_state(&tasks, &created.id, TaskState::Cancelled).await;

        assert!(cancelled.playback_source.is_none());
        assert!(cancelled.playback_session.is_none());
        assert_eq!(2, cancelled.result_items.len());
        assert!(cancelled.result_items.iter().all(|item| {
            item.state == i32::from(TaskState::Cancelled)
                && item.library_item_id.is_empty()
                && item.playback_source.is_none()
                && item.playback_session.is_none()
        }));
        assert!(state.hls_sessions.get(&created.id).is_none());
        assert!(state.hls_cache.playback_session(&created.id).is_none());
        assert!(
            !root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&created.id)
                .join("session.json")
                .exists()
        );
        assert_eq!(
            vec![("BV1range-cancel".to_owned(), None)],
            *resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
        );
        assert_eq!(
            vec![
                ("BV1range-cancel".to_owned(), Some("page:1".to_owned())),
                ("BV1range-cancel".to_owned(), Some("page:2".to_owned())),
            ],
            *playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
        );
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_cancels_during_explicit_resolution() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let resolve_requests = Arc::new(Mutex::new(Vec::new()));
        let playback_requests = Arc::new(Mutex::new(Vec::new()));
        let (resolve_started_sender, resolve_started) = oneshot::channel();
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(CancelDuringResolutionPlanner {
                resolve_requests: Arc::clone(&resolve_requests),
                playback_requests: Arc::clone(&playback_requests),
                resolve_started: Mutex::new(Some(resolve_started_sender)),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1range-resolve-cancel".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 1,
                    range_end_index: 2,
                }),
            }))
            .await
            .expect("range task should be created")
            .into_inner();

        resolve_started
            .await
            .expect("explicit resolution should begin");
        service
            .cancel_task(Request::new(CancelTaskRequest {
                id: created.id.clone(),
            }))
            .await
            .expect("explicit selection task should accept cancellation");

        let cancelled = wait_for_task_state(&tasks, &created.id, TaskState::Cancelled).await;

        assert_eq!(TaskState::Cancelled, cancelled.state());
        assert!(cancelled.playback_source.is_none());
        assert!(cancelled.playback_session.is_none());
        assert!(cancelled.result_items.is_empty());
        assert_eq!(
            vec![("BV1range-resolve-cancel".to_owned(), None)],
            *resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
        );
        assert!(
            playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_uses_later_successful_result_as_primary() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let resolve_requests = Arc::new(Mutex::new(Vec::new()));
        let playback_requests = Arc::new(Mutex::new(Vec::new()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndScriptedPlaybackPlanner {
                resolve_requests: Arc::clone(&resolve_requests),
                playback_requests: Arc::clone(&playback_requests),
                resolution: sample_resolution_with_pages(),
                results: Mutex::new(HashMap::from([
                    (
                        "page:1".to_owned(),
                        Err(BilibiliDownloadError::Failed(
                            "page 1 planning failed".to_owned(),
                        )),
                    ),
                    ("page:2".to_owned(), Ok(sample_playback_plan())),
                ])),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1range-partial-success".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 1,
                    range_end_index: 2,
                }),
            }))
            .await
            .expect("range task should be created")
            .into_inner();

        let playable = wait_for_task_state(&tasks, &created.id, TaskState::Playable).await;
        let second_result_id = format!("{}-result-2", created.id);

        assert_eq!(TaskState::Playable, playable.state());
        assert!(playable.library_item_id.is_empty());
        assert_eq!(
            second_result_id,
            playable
                .playback_session
                .as_ref()
                .expect("playable task should expose primary session")
                .id
        );
        assert_eq!(
            created.id,
            playable
                .playback_source
                .as_ref()
                .expect("playable task should expose primary source")
                .item_id
        );
        assert_eq!(2, playable.result_items.len());
        assert_eq!(i32::from(TaskState::Failed), playable.result_items[0].state);
        assert!(playable.result_items[0].playback_source.is_none());
        assert!(playable.result_items[0].playback_session.is_none());
        assert_eq!(
            i32::from(TaskState::Playable),
            playable.result_items[1].state
        );
        assert!(playable.result_items[1].playback_source.is_some());
        assert!(playable.result_items[1].playback_session.is_some());
        assert_eq!(
            vec![
                (
                    "BV1range-partial-success".to_owned(),
                    Some("page:1".to_owned())
                ),
                (
                    "BV1range-partial-success".to_owned(),
                    Some("page:2".to_owned())
                ),
            ],
            *playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
        );
        assert_eq!(
            vec![("BV1range-partial-success".to_owned(), None)],
            *resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
        );
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_finalizes_later_primary_result_cache() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndScriptedPlaybackPlanner {
                resolve_requests: Arc::new(Mutex::new(Vec::new())),
                playback_requests: Arc::new(Mutex::new(Vec::new())),
                resolution: sample_resolution_with_pages(),
                results: Mutex::new(HashMap::from([
                    (
                        "page:1".to_owned(),
                        Err(BilibiliDownloadError::Failed(
                            "page 1 planning failed".to_owned(),
                        )),
                    ),
                    (
                        "page:2".to_owned(),
                        Ok(sample_playback_plan_with_video_url(&upstream_url)),
                    ),
                ])),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let library_service = LibraryGrpcService::new(state.clone());
        let task_service = TaskGrpcService::new(state);

        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1range-secondary-cache".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 1,
                    range_end_index: 2,
                }),
            }))
            .await
            .expect("range task should be created")
            .into_inner();

        let completed = wait_for_task_state(&tasks, &created.id, TaskState::Completed).await;
        let second_result_id = format!("{}-result-2", created.id);
        let expected_item_id = format!("bilibili.hls.{second_result_id}");

        assert_eq!(expected_item_id, completed.library_item_id);
        assert_eq!(
            expected_item_id,
            completed
                .playback_source
                .as_ref()
                .expect("completed task should expose cached source")
                .item_id
        );
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[1].state
        );
        assert_eq!(expected_item_id, completed.result_items[1].library_item_id);

        let library_item = library_service
            .get_library_item(Request::new(GetLibraryItemRequest {
                id: expected_item_id,
            }))
            .await
            .expect("secondary primary completed cache should be readable")
            .into_inner();
        assert_eq!(second_result_id, library_item.source_id);
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_finalizes_secondary_result_cache() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndScriptedPlaybackPlanner {
                resolve_requests: Arc::new(Mutex::new(Vec::new())),
                playback_requests: Arc::new(Mutex::new(Vec::new())),
                resolution: sample_resolution_with_pages(),
                results: Mutex::new(HashMap::from([
                    (
                        "page:1".to_owned(),
                        Ok(sample_playback_plan_with_video_url(&upstream_url)),
                    ),
                    (
                        "page:2".to_owned(),
                        Ok(sample_playback_plan_with_video_url(&upstream_url)),
                    ),
                ])),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let library_service = LibraryGrpcService::new(state.clone());
        let task_service = TaskGrpcService::new(state);

        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1range-secondary-finalization".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 1,
                    range_end_index: 2,
                }),
            }))
            .await
            .expect("range task should be created")
            .into_inner();

        let second_result_id = format!("{}-result-2", created.id);
        let primary_item_id = format!("bilibili.hls.{}", created.id);
        let secondary_item_id = format!("bilibili.hls.{second_result_id}");
        let completed = wait_for_task_condition(&tasks, &created.id, |task| {
            task.state() == TaskState::Completed
                && task.result_items.len() == 2
                && task.result_items[1].state == i32::from(TaskState::Completed)
        })
        .await;

        assert_eq!(primary_item_id, completed.library_item_id);
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[0].state
        );
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[1].state
        );
        assert_eq!(secondary_item_id, completed.result_items[1].library_item_id);

        let library_item = library_service
            .get_library_item(Request::new(GetLibraryItemRequest {
                id: secondary_item_id,
            }))
            .await
            .expect("secondary completed cache should be readable")
            .into_inner();
        assert_eq!(second_result_id, library_item.source_id);
    }

    #[tokio::test]
    async fn completed_playback_task_keeps_runtime_hls_alternates_for_stale_clients() {
        let (selected_upstream_url, _selected_upstream_task) = start_mp4_upstream().await;
        let (alternate_upstream_url, _alternate_upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let (planner, planner_started, plan_sender) = DeferredPlaybackPlanner::new();
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(planner),
        );
        let service = TaskGrpcService::new(state.clone());

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1runtime-alternate".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();
        planner_started
            .await
            .expect("background playback planner should start");
        plan_sender
            .send(Ok(sample_playback_plan_with_alternate_video_urls(
                &selected_upstream_url,
                &alternate_upstream_url,
            )))
            .expect("test should send playback plan");

        let completed = wait_for_task_state(&state.tasks, &created.id, TaskState::Completed).await;
        let runtime_session = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(session) = state.hls_sessions.get(&completed.id)
                    && session.variant.video.request.url.is_empty()
                {
                    break session;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("completed runtime HLS session should remain registered");

        assert_eq!(1, runtime_session.alternate_variants.len());
        assert!(
            !runtime_session
                .master_playlist()
                .contains("segments/v1-video.m3u8")
        );
        assert!(
            runtime_session
                .media_playlist_resource("v1-video.m3u8")
                .is_some()
        );
        let runtime_alternate = runtime_session
            .media_resource("v1-video.m4s")
            .expect("runtime alternate media resource should remain serveable");
        assert_eq!(alternate_upstream_url, runtime_alternate.request.url);
        assert!(runtime_alternate.request.backup_urls.is_empty());
        assert!(!runtime_alternate.request.headers.is_empty());

        let persisted_session = state
            .hls_cache
            .completed_session(&completed.id)
            .expect("completed HLS session should persist");
        assert_eq!(1, persisted_session.alternate_variants.len());
        assert!(
            !persisted_session
                .master_playlist()
                .contains("segments/v1-video.m3u8")
        );
        assert!(
            persisted_session
                .media_playlist_resource("v1-video.m3u8")
                .is_some()
        );
        let persisted_alternate = persisted_session
            .media_resource("v1-video.m4s")
            .expect("persisted alternate media resource should remain serveable");
        assert!(persisted_alternate.request.url.is_empty());
        assert!(persisted_alternate.request.backup_urls.is_empty());
        assert!(persisted_alternate.request.headers.is_empty());
    }

    #[tokio::test]
    async fn create_bilibili_playback_task_fails_stale_selection_without_planning() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let resolve_requests = Arc::new(Mutex::new(Vec::new()));
        let playback_requests = Arc::new(Mutex::new(Vec::new()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(StaticResolveAndRecordingPlaybackPlanner {
                resolve_requests: Arc::clone(&resolve_requests),
                playback_requests: Arc::clone(&playback_requests),
                resolution: sample_resolution_with_pages(),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let service = TaskGrpcService::new(state);

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1stale".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_SINGLE,
                    selection_ids: vec!["page:404".to_owned()],
                    range_start_index: 0,
                    range_end_index: 0,
                }),
            }))
            .await
            .expect("stale selection task should be created")
            .into_inner();

        let failed = wait_for_task_state(&tasks, &created.id, TaskState::Failed).await;

        assert!(failed.message.contains("was not found"));
        assert_eq!(
            vec![("BV1stale".to_owned(), None)],
            *resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
        );
        assert!(
            playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn explicit_item_ids_recover_when_a_refreshed_feed_no_longer_contains_the_item() {
        let selection_id = "item:7:source:recommendation:cid:270001:bvid:BV1xx411c7mD:aid:170001";
        let resolution = BilibiliInputResolution {
            source: "https://www.bilibili.com/".to_owned(),
            title: "Refreshed recommendations".to_owned(),
            source_kind: "recommendation".to_owned(),
            candidates: vec![AdapterBilibiliResolvedCandidate {
                selection_id:
                    "item:1:source:recommendation:cid:270002:bvid:BV1yy411c7mD:aid:170002"
                        .to_owned(),
                title: "Different recommendation".to_owned(),
                subtitle: String::new(),
                source_kind: "recommendation".to_owned(),
                content_id: "BV1yy411c7mD".to_owned(),
                index: 1,
                duration_seconds: Some(60),
                cover_uri: String::new(),
            }],
            default_selection_id: String::new(),
            candidates_truncated: false,
        };

        let selected = selected_bilibili_candidates(
            &resolution,
            &BilibiliPlaybackSelectionPlanMode::ExplicitIds {
                selection_ids: vec![selection_id.to_owned()],
            },
        )
        .expect("server-owned stable item identity should survive feed refresh");

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].selection_id, selection_id);
        assert_eq!(selected[0].content_id, "BV1xx411c7mD");
        assert_eq!(selected[0].index, 7);
    }

    #[test]
    fn explicit_item_ids_reject_selection_bound_to_another_source() {
        let resolution = BilibiliInputResolution {
            source: "https://www.bilibili.com/".to_owned(),
            title: "Refreshed recommendations".to_owned(),
            source_kind: "recommendation".to_owned(),
            candidates: Vec::new(),
            default_selection_id: String::new(),
            candidates_truncated: false,
        };

        let error = selected_bilibili_candidates(
            &resolution,
            &BilibiliPlaybackSelectionPlanMode::ExplicitIds {
                selection_ids: vec![
                    "item:7:source:history:cid:270001:bvid:BV1xx411c7mD:aid:170001".to_owned(),
                ],
            },
        )
        .expect_err("cross-source stable selection should be rejected");

        assert!(error.contains("was not found"));
    }

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
                        audio_language: "ja-jp".to_owned(),
                        playback_policy: None,
                    }),
                    selection_id: String::new(),
                    selection: None,
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
        assert_eq!(
            PlaybackPolicy::default().to_proto(),
            session
                .effective_policy
                .expect("playback session should expose its effective policy")
        );
        let transcoding_plan = session
            .transcoding_plan
            .as_ref()
            .expect("playback session should expose a LAN transcoding plan");
        assert_eq!(
            LanTranscodingPlanState::NotRequired as i32,
            transcoding_plan.state
        );
        assert_eq!("avplayer-h264-aac-hls-v1", transcoding_plan.profile_id);
        assert_eq!(
            PlaybackProtocol::Hls as i32,
            transcoding_plan.output_protocol
        );
        assert_eq!("dash", session.selected_variant.unwrap().source_kind);
        let hls_session = service
            .state
            .hls_sessions
            .get(&task.id)
            .expect("runtime HLS session should exist");
        assert_eq!("h264", hls_session.variant.id);
        assert_eq!(
            HlsTranscodingPlanState::NotRequired,
            hls_session.transcoding.state
        );
        assert_eq!(1, hls_session.abr.groups.len());
        assert_eq!("dash-video", hls_session.abr.groups[0].id);
        assert_eq!(vec!["h264", "hevc"], hls_session.abr.groups[0].variant_ids);
        assert_eq!(2, hls_session.variants.len());
        assert_eq!(
            "dash-video",
            hls_session.variants[0].abr.as_ref().unwrap().group_id
        );
        assert_eq!(
            "source-hash",
            hls_session.variants[0].media[0].cache_key.source_hash
        );
        let restored_hls_session = service
            .state
            .hls_cache
            .playback_session(&task.id)
            .expect("persisted HLS session should exist");
        assert_eq!(hls_session.abr, restored_hls_session.abr);
        assert_eq!(hls_session.variants, restored_hls_session.variants);
        assert_eq!(hls_session.transcoding, restored_hls_session.transcoding);
        assert_eq!(
            PlaybackPolicy::default(),
            restored_hls_session.effective_policy
        );

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

    #[test]
    fn playback_metadata_marks_non_h264_variant_ready_for_lan_transcoding_when_enabled() {
        let mut plan = sample_playback_plan();
        let selected_hevc = plan.entries[0].variants[1].clone();
        plan.entries[0].selected_variant = Some(BilibiliSelectedPlaybackVariant {
            variant: selected_hevc,
            selection: BilibiliPlaybackVariantSelection {
                policy: BilibiliPlaybackVariantSelectionPolicy::ExplicitEncodingPreference,
                codec_rank: Some(1),
                score: 100,
            },
        });

        let metadata = playback_task_metadata_with_options(
            "bilibili-playback-transcoding",
            plan,
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
        )
        .expect("playback metadata should map");

        assert_eq!(
            HlsTranscodingPlanState::Ready,
            metadata.hls_session.transcoding.state
        );
        let proto_plan = metadata
            .playback_session
            .transcoding_plan
            .expect("proto playback session should include a transcoding plan");
        assert_eq!(LanTranscodingPlanState::Ready as i32, proto_plan.state);
        assert_eq!("hevc", proto_plan.source_variant_id);
        assert_eq!(PlaybackProtocol::Hls as i32, proto_plan.output_protocol);
    }

    #[test]
    fn playback_metadata_returns_normalized_effective_policy() {
        let mut plan = sample_playback_plan();
        let selected_hevc = plan.entries[0].variants[1].clone();
        plan.entries[0].selected_variant = Some(BilibiliSelectedPlaybackVariant {
            variant: selected_hevc,
            selection: BilibiliPlaybackVariantSelection {
                policy: BilibiliPlaybackVariantSelectionPolicy::ExplicitEncodingPreference,
                codec_rank: Some(1),
                score: 100,
            },
        });
        let policy = PlaybackPolicy {
            transcoding_preference: TranscodingPreference::Never,
            compatible_variant_preference: CompatibleVariantPreference::PreferRequested,
            weak_network_preference: WeakNetworkPreference::HoldDowngrade,
        };

        let metadata = playback_task_metadata_with_policy(
            "bilibili-playback-policy",
            plan,
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            policy,
        )
        .expect("playback metadata should map");

        assert_eq!(policy, metadata.hls_session.effective_policy);
        assert_eq!(
            HlsTranscodingPlanState::Disabled,
            metadata.hls_session.transcoding.state
        );
        assert_eq!(
            Some(policy.to_proto()),
            metadata.playback_session.effective_policy
        );
        assert_eq!(
            Some(LanTranscodingPlanState::Disabled),
            metadata
                .playback_session
                .transcoding_plan
                .as_ref()
                .map(|plan| plan.state())
        );
    }

    #[tokio::test]
    async fn cancelled_playback_planning_does_not_persist_hls_manifest() {
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
        let service = TaskGrpcService::new(state.clone());

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1cancel-before-manifest".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();
        planner_started
            .await
            .expect("background playback planner should start");
        tasks
            .cancel_task(&created.id)
            .expect("task should be cancellable while planner is pending");
        plan_sender
            .send(Ok(sample_playback_plan()))
            .expect("test should send playback plan");

        let cancelled = wait_for_task_state(&tasks, &created.id, TaskState::Cancelled).await;

        assert!(cancelled.playback_source.is_none());
        assert!(cancelled.playback_session.is_none());
        assert!(state.hls_sessions.get(&created.id).is_none());
        assert!(
            !root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&created.id)
                .join("session.json")
                .exists()
        );
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
                selection_id: String::new(),
                selection: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();
        let first = wait_for_task_state(&tasks, &first_created.id, TaskState::Failed).await;
        let second_created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1empty".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
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
                    selection_id: String::new(),
                    selection: None,
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
    async fn create_playback_task_registers_background_work_before_spawn_poll() {
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
        let service = TaskGrpcService::new(state.clone());

        let created = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1registered-before-poll".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
            }))
            .await
            .expect("playback task should be created")
            .into_inner();

        assert!(!state.background_work_is_idle());
        tasks
            .cancel_task(&created.id)
            .expect("pending planning task should accept cancellation");
        let cancelled = wait_for_task_state(&tasks, &created.id, TaskState::Cancelled).await;
        assert_eq!(TaskState::Cancelled, cancelled.state());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !state.background_work_is_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled planning task should release background activity");
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
                selection_id: String::new(),
                selection: None,
            }))
            .await
            .expect("first playback task should be created")
            .into_inner();
        let second = service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1pending-duplicate".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
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
                selection_id: String::new(),
                selection: None,
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
                selection_id: String::new(),
                selection: None,
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
                selection_id: String::new(),
                selection: None,
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

        let restored_options = CacheServerOptions {
            public_media_base_uri: Some("http://restored-media.example.test:9090".to_owned()),
            ..options.clone()
        };
        let restored =
            AppState::new_with_playback_planner(restored_options, Arc::new(EmptyPlaybackPlanner));
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
        assert_eq!(
            format!(
                "http://restored-media.example.test:9090/hls/{}/master.m3u8",
                completed.id
            ),
            restored_task
                .playback_source
                .as_ref()
                .expect("restored completed task should keep offline source")
                .uri
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
    async fn delete_library_item_removes_completed_hls_cache_and_task_record() {
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
            allow_library_item_delete: true,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let (planner, planner_started, plan_sender) = DeferredPlaybackPlanner::new();
        let state = AppState::new_with_playback_planner(options.clone(), Arc::new(planner));
        let task_service = TaskGrpcService::new(state.clone());
        let cache_service = CacheGrpcService::new(state.clone());
        let library_service = LibraryGrpcService::new(state.clone());

        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1deletehls".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
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

        let completed = wait_for_task_state(&state.tasks, &created.id, TaskState::Completed).await;
        let expected_item_id = format!("bilibili.hls.{}", completed.id);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );
        let generation = state
            .hls_sessions
            .get_with_generation(&completed.id)
            .expect("completed HLS session should remain registered")
            .generation;
        state.hls_network_policy.record_upstream_failure_for_policy(
            WeakNetworkPreference::Adaptive,
            &completed.id,
            generation,
            "h264",
        );
        assert_eq!(
            RuntimeTestHlsWeakNetworkState::UpstreamFailed,
            state.hls_weak_network_status().state
        );

        let deleted = cache_service
            .delete_library_item(Request::new(DeleteLibraryItemRequest {
                id: expected_item_id.clone(),
            }))
            .await
            .expect("completed HLS cache item should delete")
            .into_inner();
        assert!(deleted.deleted);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_none()
        );
        assert!(state.hls_sessions.get(&completed.id).is_none());
        assert_eq!(
            RuntimeTestHlsWeakNetworkState::Normal,
            state.hls_weak_network_status().state
        );
        assert!(state.tasks.get_task(&completed.id).is_err());
        assert!(
            !root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&completed.id)
                .exists()
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
            .expect("library list should succeed")
            .into_inner();
        assert!(library.items.is_empty());

        let repeated_delete = cache_service
            .delete_library_item(Request::new(DeleteLibraryItemRequest {
                id: expected_item_id.clone(),
            }))
            .await
            .expect("second delete should be idempotent")
            .into_inner();
        assert!(!repeated_delete.deleted);

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        assert!(restored.tasks.get_task(&completed.id).is_err());
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_none()
        );
    }

    #[tokio::test]
    async fn delete_library_item_removes_sibling_result_hls_sessions() {
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
                allow_library_item_delete: true,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let cache_service = CacheGrpcService::new(state.clone());
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1delete-multi-hls", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let primary_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("child playback metadata should map");
        state
            .hls_cache
            .save_session(&primary_metadata.hls_session)
            .expect("primary HLS session should persist");
        state
            .hls_cache
            .save_session(&child_metadata.hls_session)
            .expect("child HLS session should persist");
        state
            .hls_sessions
            .insert(primary_metadata.hls_session.clone());
        state
            .hls_sessions
            .insert(child_metadata.hls_session.clone());

        let primary_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                "Multi Result".to_owned(),
                "All results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: creation.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session.clone()),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session.clone()),
                    },
                ],
            )
            .expect("multi-result playback task should become playable");
        let library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &primary_metadata.hls_session)
            .await
            .expect("primary HLS resources should cache");
        state
            .tasks
            .complete_playback_hls_session_cached(
                &creation.task.id,
                &creation.task.id,
                library_item_id.clone(),
            )
            .expect("primary HLS session should become completed");
        state
            .hls_sessions
            .insert(sanitized_completed_session(&primary_metadata.hls_session));
        assert!(state.hls_sessions.get(&child_session_id).is_some());
        assert!(
            state
                .hls_cache
                .playback_session(&child_session_id)
                .is_some()
        );

        let deleted = cache_service
            .delete_library_item(Request::new(DeleteLibraryItemRequest {
                id: library_item_id,
            }))
            .await
            .expect("completed HLS cache item should delete")
            .into_inner();

        assert!(deleted.deleted);
        assert!(state.tasks.get_task(&creation.task.id).is_err());
        assert!(state.hls_sessions.get(&creation.task.id).is_none());
        assert!(state.hls_sessions.get(&child_session_id).is_none());
        assert!(
            state
                .hls_cache
                .playback_session(&creation.task.id)
                .is_none()
        );
        assert!(
            state
                .hls_cache
                .playback_session(&child_session_id)
                .is_none()
        );
        assert!(
            !root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&creation.task.id)
                .exists()
        );
        assert!(
            !root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&child_session_id)
                .exists()
        );

        let stale_child_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(state)),
            AxumPath(child_session_id),
        )
        .await;
        assert_eq!(StatusCode::NOT_FOUND, stale_child_master.status());
    }

    #[tokio::test]
    async fn delete_library_item_removes_only_completed_secondary_result_hls_session() {
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
                allow_library_item_delete: true,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let cache_service = CacheGrpcService::new(state.clone());
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1delete-secondary-hls-only", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let primary_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("child playback metadata should map");
        state
            .hls_cache
            .save_session(&primary_metadata.hls_session)
            .expect("primary HLS session should persist");
        state
            .hls_cache
            .save_session(&child_metadata.hls_session)
            .expect("child HLS session should persist");
        state
            .hls_sessions
            .insert(primary_metadata.hls_session.clone());
        state
            .hls_sessions
            .insert(child_metadata.hls_session.clone());

        let primary_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                "Multi Result".to_owned(),
                "All results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: creation.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session.clone()),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session.clone()),
                    },
                ],
            )
            .expect("multi-result playback task should become playable");
        let primary_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &primary_metadata.hls_session)
            .await
            .expect("primary HLS resources should cache");
        state
            .tasks
            .complete_playback_hls_session_cached(
                &creation.task.id,
                &creation.task.id,
                primary_library_item_id.clone(),
            )
            .expect("primary HLS session should become completed");
        state
            .hls_sessions
            .insert(sanitized_completed_session(&primary_metadata.hls_session));
        let child_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &child_metadata.hls_session)
            .await
            .expect("child HLS resources should cache");
        state
            .tasks
            .complete_playback_hls_session_cached(
                &creation.task.id,
                &child_session_id,
                child_library_item_id.clone(),
            )
            .expect("child HLS session should become completed");
        state
            .hls_sessions
            .insert(sanitized_completed_session(&child_metadata.hls_session));

        let deleted = cache_service
            .delete_library_item(Request::new(DeleteLibraryItemRequest {
                id: child_library_item_id.clone(),
            }))
            .await
            .expect("completed secondary HLS cache item should delete")
            .into_inner();

        assert!(deleted.deleted);
        let task = state
            .tasks
            .get_task(&creation.task.id)
            .expect("completed parent task should remain");
        assert_eq!(TaskState::Completed, task.state());
        assert_eq!(primary_library_item_id, task.library_item_id);
        assert_eq!(i32::from(TaskState::Completed), task.result_items[0].state);
        assert_eq!(i32::from(TaskState::Failed), task.result_items[1].state);
        assert!(task.result_items[1].library_item_id.is_empty());
        assert!(task.result_items[1].playback_source.is_none());
        assert!(task.result_items[1].playback_session.is_none());
        assert!(state.hls_sessions.get(&creation.task.id).is_some());
        assert!(state.hls_sessions.get(&child_session_id).is_none());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&primary_library_item_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&child_library_item_id)
                .is_none()
        );
        assert!(
            state
                .hls_cache
                .playback_session(&creation.task.id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .playback_session(&child_session_id)
                .is_none()
        );
        assert!(
            root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&creation.task.id)
                .exists()
        );
        assert!(
            !root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&child_session_id)
                .exists()
        );
        let primary_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(state.clone())),
            AxumPath(creation.task.id.clone()),
        )
        .await;
        assert_eq!(StatusCode::OK, primary_master.status());
        let stale_child_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(state)),
            AxumPath(child_session_id),
        )
        .await;
        assert_eq!(StatusCode::NOT_FOUND, stale_child_master.status());
    }

    #[tokio::test]
    async fn get_hls_cache_status_reports_quota_and_usage() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: 1_000,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 80,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        create_completed_hls_playback_task(&state, "BV1status", &upstream_url).await;
        let service = CacheGrpcService::new(state);

        let status = service
            .get_hls_cache_status(Request::new(GetHlsCacheStatusRequest {}))
            .await
            .expect("HLS cache status should load")
            .into_inner();

        assert!(status.eviction_enabled);
        assert_eq!(1_000, status.max_bytes);
        assert_eq!(90, status.high_watermark_percent);
        assert_eq!(80, status.low_watermark_percent);
        assert_eq!(900, status.high_watermark_bytes);
        assert_eq!(800, status.low_watermark_bytes);
        assert_eq!(fake_mp4().len() as i64, status.used_bytes);
        assert_eq!(1, status.completed_session_count);
        assert!(status.last_eviction.is_none());
        let weak_network = status
            .weak_network
            .expect("weak network policy status should be present");
        assert_eq!(HlsWeakNetworkState::Normal as i32, weak_network.state);
        assert_eq!("HLS upstream policy normal.", weak_network.message);
        assert_eq!(0, weak_network.unhealthy_variant_count);
        let transcoding = status
            .transcoding
            .expect("LAN transcoding status should be present");
        assert!(!transcoding.enabled);
        assert_eq!(
            ProtoLanTranscodingRuntimeState::Disabled as i32,
            transcoding.state
        );
        assert_eq!("avplayer-h264-aac-hls-v1", transcoding.profile_id);
        let playback = status
            .playback
            .expect("HLS playback progress status should be present");
        assert_eq!(ProtoHlsPlaybackActivityState::None as i32, playback.state);
        assert_eq!(
            "No active HLS playback position reported.",
            playback.message
        );
    }

    #[tokio::test]
    async fn get_hls_cache_status_reports_weak_network_policy() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        state
            .hls_network_policy
            .record_upstream_failure("session-1", "1080p");
        let service = CacheGrpcService::new(state);

        let status = service
            .get_hls_cache_status(Request::new(GetHlsCacheStatusRequest {}))
            .await
            .expect("HLS cache status should load")
            .into_inner();

        let weak_network = status
            .weak_network
            .expect("weak network policy status should be present");
        assert_eq!(
            HlsWeakNetworkState::UpstreamFailed as i32,
            weak_network.state
        );
        assert_eq!(1, weak_network.degraded_session_count);
        assert_eq!(1, weak_network.unhealthy_variant_count);
    }

    #[tokio::test]
    async fn report_playback_progress_updates_hls_cache_status() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let (task_id, _hls_session, _library_item_id) =
            create_playable_hls_playback_task(&state, "BV1playback-progress", &upstream_url);
        let service = CacheGrpcService::new(state);

        let report = service
            .report_playback_progress(Request::new(ReportPlaybackProgressRequest {
                playback_uri: format!("http://media.example.test:8080/hls/{task_id}/master.m3u8"),
                library_item_id: String::new(),
                variant_id: "h264".to_owned(),
                position_seconds: 42.0,
                duration_seconds: 120.0,
                intent: ProtoPlaybackProgressIntent::Seek.into(),
            }))
            .await
            .expect("playback progress should be accepted")
            .into_inner();

        assert!(report.accepted);
        assert_eq!(task_id, report.session_id);

        let status = service
            .get_hls_cache_status(Request::new(GetHlsCacheStatusRequest {}))
            .await
            .expect("HLS cache status should load")
            .into_inner();
        let playback = status
            .playback
            .expect("HLS playback progress status should be present");
        assert_eq!(ProtoHlsPlaybackActivityState::Active as i32, playback.state);
        assert_eq!(task_id, playback.session_id);
        assert_eq!("h264", playback.variant_id);
        assert_eq!(42.0, playback.position_seconds);
        assert_eq!(120.0, playback.duration_seconds);
        assert_eq!(
            ProtoPlaybackProgressIntent::Seek as i32,
            playback.last_intent
        );
    }

    #[tokio::test]
    async fn report_playback_progress_promotes_demoted_hls_cache_fill() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let (active_task_id, active_hls_session, _active_library_item_id) =
            create_playable_hls_playback_task(&state, "BV1active-fill", &upstream_url);
        let (demoted_task_id, demoted_hls_session, _demoted_library_item_id) =
            create_playable_hls_playback_task(&state, "BV1demoted-fill", &upstream_url);
        assert!(state.hls_fill_scheduler.enqueue_foreground(
            active_task_id.clone(),
            active_hls_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let active_job = state.hls_fill_scheduler.next_job().await;
        assert!(!state.hls_fill_scheduler.enqueue_demoted(
            demoted_task_id.clone(),
            demoted_hls_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let service = CacheGrpcService::new(state.clone());

        let report = service
            .report_playback_progress(Request::new(ReportPlaybackProgressRequest {
                playback_uri: format!(
                    "http://media.example.test:8080/hls/{demoted_task_id}/master.m3u8"
                ),
                library_item_id: String::new(),
                variant_id: "h264".to_owned(),
                position_seconds: 90.0,
                duration_seconds: 180.0,
                intent: ProtoPlaybackProgressIntent::Seek.into(),
            }))
            .await
            .expect("playback progress should be accepted")
            .into_inner();

        assert!(report.accepted);
        assert!(active_job.token.is_preempted());
        state.hls_fill_scheduler.finish_current(&active_job, false);
        let promoted = state.hls_fill_scheduler.next_job().await;
        assert_eq!(demoted_task_id, promoted.task_id);
        assert_eq!(
            crate::hls_fill_scheduler::HlsFillPriority::Foreground,
            promoted.priority
        );
    }

    #[tokio::test]
    async fn report_playback_progress_rejects_unknown_hls_cache_session() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let service = CacheGrpcService::new(state);

        let report = service
            .report_playback_progress(Request::new(ReportPlaybackProgressRequest {
                playback_uri: "http://media.example.test:8080/hls/unknown-session/master.m3u8"
                    .to_owned(),
                library_item_id: String::new(),
                variant_id: "h264".to_owned(),
                position_seconds: 42.0,
                duration_seconds: 120.0,
                intent: ProtoPlaybackProgressIntent::Playing.into(),
            }))
            .await
            .expect("unknown playback progress should return a result")
            .into_inner();

        assert!(!report.accepted);
        assert_eq!("unknown-session", report.session_id);

        let status = service
            .get_hls_cache_status(Request::new(GetHlsCacheStatusRequest {}))
            .await
            .expect("HLS cache status should load")
            .into_inner();
        let playback = status
            .playback
            .expect("HLS playback progress status should be present");
        assert_eq!(ProtoHlsPlaybackActivityState::None as i32, playback.state);
        assert_eq!("", playback.session_id);
    }

    #[tokio::test]
    async fn removed_hls_session_clears_playback_progress_status() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let (task_id, _hls_session, _library_item_id) =
            create_playable_hls_playback_task(&state, "BV1removed-progress", &upstream_url);
        let service = CacheGrpcService::new(state.clone());

        service
            .report_playback_progress(Request::new(ReportPlaybackProgressRequest {
                playback_uri: format!("http://media.example.test:8080/hls/{task_id}/master.m3u8"),
                library_item_id: String::new(),
                variant_id: "h264".to_owned(),
                position_seconds: 42.0,
                duration_seconds: 120.0,
                intent: ProtoPlaybackProgressIntent::Playing.into(),
            }))
            .await
            .expect("playback progress should be accepted");

        state.remove_hls_playback_session(&task_id);

        let status = service
            .get_hls_cache_status(Request::new(GetHlsCacheStatusRequest {}))
            .await
            .expect("HLS cache status should load")
            .into_inner();
        let playback = status
            .playback
            .expect("HLS playback progress status should be present");
        assert_eq!(ProtoHlsPlaybackActivityState::None as i32, playback.state);
        assert_eq!("", playback.session_id);
    }

    #[tokio::test]
    async fn hls_cache_quota_evicts_oldest_completed_session_to_low_watermark() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 2,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 50,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let first =
            create_completed_hls_playback_task(&state, "BV1first-cache", &upstream_url).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second =
            create_completed_hls_playback_task(&state, "BV1second-cache", &upstream_url).await;

        let summary = state
            .enforce_hls_cache_quota("test", Vec::new(), 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger eviction");

        assert_eq!(2 * session_size, summary.started_used_bytes);
        assert_eq!(session_size, summary.finished_used_bytes);
        assert_eq!(session_size, summary.target_used_bytes);
        assert_eq!(session_size, summary.evicted_bytes);
        assert_eq!(vec![first.task_id.clone()], summary.evicted_session_ids);
        assert!(summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&first.library_item_id)
                .is_none()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&second.library_item_id)
                .is_some()
        );
        assert!(state.tasks.get_task(&first.task_id).is_err());
        assert!(state.tasks.get_task(&second.task_id).is_ok());
        let status = state
            .hls_cache_status()
            .expect("status should scan after eviction");
        assert_eq!(Some(summary), status.last_eviction);
    }

    #[tokio::test]
    async fn hls_cache_quota_skips_protected_completed_session() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 2,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 50,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let protected =
            create_completed_hls_playback_task(&state, "BV1protected-cache", &upstream_url).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let evictable =
            create_completed_hls_playback_task(&state, "BV1evictable-cache", &upstream_url).await;

        let summary = state
            .enforce_hls_cache_quota("test", [protected.task_id.clone()], 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger eviction");

        assert_eq!(vec![evictable.task_id.clone()], summary.evicted_session_ids);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&protected.library_item_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&evictable.library_item_id)
                .is_none()
        );
        assert!(state.tasks.get_task(&protected.task_id).is_ok());
        assert!(state.tasks.get_task(&evictable.task_id).is_err());
    }

    #[tokio::test]
    async fn hls_cache_quota_evicts_unprotected_session_when_protected_usage_exceeds_low_watermark()
    {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 3,
                hls_cache_high_watermark_percent: 50,
                hls_cache_low_watermark_percent: 25,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let old =
            create_completed_hls_playback_task(&state, "BV1old-unprotected", &upstream_url).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let protected =
            create_completed_hls_playback_task(&state, "BV1large-protected", &upstream_url).await;

        let summary = state
            .enforce_hls_cache_quota("test", [protected.task_id.clone()], 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger eviction");

        assert_eq!(vec![old.task_id.clone()], summary.evicted_session_ids);
        assert_eq!(session_size, summary.finished_used_bytes);
        assert!(!summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&old.library_item_id)
                .is_none()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&protected.library_item_id)
                .is_some()
        );
        assert!(state.tasks.get_task(&old.task_id).is_err());
        assert!(state.tasks.get_task(&protected.task_id).is_ok());
    }

    #[tokio::test]
    async fn hls_cache_quota_skips_recent_completed_playback_source() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                hls_cache_max_bytes: session_size * 2,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 50,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let recent =
            create_completed_hls_playback_task(&state, "BV1recent-cache", &upstream_url).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let evictable =
            create_completed_hls_playback_task(&state, "BV1unused-cache", &upstream_url).await;

        let source = state
            .create_completed_hls_playback_source(
                &recent.library_item_id,
                "h264",
                format!(
                    "http://media.example.test:8080/hls/{}/master.m3u8",
                    recent.task_id
                ),
            )
            .expect("completed HLS item should produce a playback source");
        assert_eq!(recent.library_item_id, source.item_id);

        let summary = state
            .enforce_hls_cache_quota("test", Vec::new(), 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger eviction");

        assert_eq!(vec![evictable.task_id.clone()], summary.evicted_session_ids);
        assert!(summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&recent.library_item_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&evictable.library_item_id)
                .is_none()
        );
        assert!(state.tasks.get_task(&recent.task_id).is_ok());
        assert!(state.tasks.get_task(&evictable.task_id).is_err());
    }

    #[tokio::test]
    async fn hls_cache_quota_protects_playable_secondary_result_after_primary_completed() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                hls_cache_max_bytes: session_size * 3,
                hls_cache_high_watermark_percent: 50,
                hls_cache_low_watermark_percent: 0,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1quota-protected-secondary", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let primary_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("child playback metadata should map");
        state
            .hls_cache
            .save_session(&primary_metadata.hls_session)
            .expect("primary HLS session should persist");
        state
            .hls_cache
            .save_session(&child_metadata.hls_session)
            .expect("child HLS session should persist");
        state
            .hls_sessions
            .insert(primary_metadata.hls_session.clone());
        state
            .hls_sessions
            .insert(child_metadata.hls_session.clone());
        let primary_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                "Multi Result".to_owned(),
                "All results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: creation.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session.clone()),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session.clone()),
                    },
                ],
            )
            .expect("multi-result playback task should become playable");
        let primary_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &primary_metadata.hls_session)
            .await
            .expect("primary HLS resources should cache");
        state
            .tasks
            .complete_playback_hls_session_cached(
                &creation.task.id,
                &creation.task.id,
                primary_library_item_id.clone(),
            )
            .expect("primary HLS session should become completed");
        state
            .hls_sessions
            .insert(sanitized_completed_session(&primary_metadata.hls_session));

        let child_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &child_metadata.hls_session)
            .await
            .expect("child HLS resources should cache");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let evictable =
            create_completed_hls_playback_task(&state, "BV1quota-evictable", &upstream_url).await;

        let summary = state
            .enforce_hls_cache_quota("test", [child_session_id.clone()], 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger eviction");

        assert_eq!(vec![evictable.task_id.clone()], summary.evicted_session_ids);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&primary_library_item_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&child_library_item_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&evictable.library_item_id)
                .is_none()
        );
        assert!(state.tasks.get_task(&creation.task.id).is_ok());
        assert!(state.hls_sessions.get(&child_session_id).is_some());
    }

    #[test]
    fn completed_hls_cleanup_item_accepts_playable_secondary_result_cache() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1cleanup-playable-secondary", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let primary_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url("http://media.example.test/video-primary.m4s"),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url("http://media.example.test/video-child.m4s"),
        )
        .expect("child playback metadata should map");
        let primary_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                "Multi Result".to_owned(),
                "All results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: creation.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session),
                    },
                ],
            )
            .expect("multi-result playback task should become playable");
        let child_library_item_id = format!("bilibili.hls.{child_session_id}");
        let playable = state
            .tasks
            .complete_playback_hls_session_cached(
                &creation.task.id,
                &child_session_id,
                child_library_item_id.clone(),
            )
            .expect("secondary result should become completed while parent stays playable");

        assert_eq!(TaskState::Playable, playable.state());
        assert_eq!(
            i32::from(TaskState::Completed),
            playable.result_items[1].state
        );
        assert_eq!(
            Some((child_session_id.clone(), child_library_item_id.clone())),
            state.completed_hls_task_cleanup_item_for_tests(
                &child_session_id,
                &child_library_item_id,
            )
        );
    }

    #[tokio::test]
    async fn hls_cache_quota_accounts_for_grouped_completed_result_sessions() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                hls_cache_max_bytes: session_size * 3,
                hls_cache_high_watermark_percent: 50,
                hls_cache_low_watermark_percent: 0,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1quota-grouped-secondary", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let primary_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("child playback metadata should map");
        state
            .hls_cache
            .save_session(&primary_metadata.hls_session)
            .expect("primary HLS session should persist");
        state
            .hls_cache
            .save_session(&child_metadata.hls_session)
            .expect("child HLS session should persist");
        state
            .hls_sessions
            .insert(primary_metadata.hls_session.clone());
        state
            .hls_sessions
            .insert(child_metadata.hls_session.clone());
        let primary_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                "Multi Result".to_owned(),
                "All results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: creation.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session.clone()),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session.clone()),
                    },
                ],
            )
            .expect("multi-result playback task should become playable");
        let child_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &child_metadata.hls_session)
            .await
            .expect("child HLS resources should cache");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let primary_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &primary_metadata.hls_session)
            .await
            .expect("primary HLS resources should cache");
        state
            .tasks
            .complete_playback_hls_session_cached(
                &creation.task.id,
                &creation.task.id,
                primary_library_item_id.clone(),
            )
            .expect("primary HLS session should become completed");
        state
            .hls_sessions
            .insert(sanitized_completed_session(&primary_metadata.hls_session));

        let summary = state
            .enforce_hls_cache_quota("test", Vec::new(), 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger eviction");

        assert_eq!(2 * session_size, summary.started_used_bytes);
        assert_eq!(0, summary.finished_used_bytes);
        assert_eq!(0, summary.target_used_bytes);
        assert_eq!(2 * session_size, summary.evicted_bytes);
        assert_eq!(
            vec![creation.task.id.clone(), child_session_id.clone()],
            summary.evicted_session_ids
        );
        assert!(summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&primary_library_item_id)
                .is_none()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&child_library_item_id)
                .is_none()
        );
        assert!(state.tasks.get_task(&creation.task.id).is_err());
    }

    #[tokio::test]
    async fn hls_cache_quota_rechecks_finalization_protection_before_eviction() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 2,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 50,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let just_used =
            create_completed_hls_playback_task(&state, "BV1late-lease", &upstream_url).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let evictable =
            create_completed_hls_playback_task(&state, "BV1late-evict", &upstream_url).await;
        let should_cancel_calls = std::cell::Cell::new(0_usize);
        let late_guard = std::cell::RefCell::new(None);

        let summary = state
            .enforce_hls_cache_quota_until_cancelled("test", Vec::new(), 0, || {
                let call = should_cancel_calls.get();
                should_cancel_calls.set(call + 1);
                if call == 3 {
                    *late_guard.borrow_mut() =
                        Some(state.protect_hls_cache_session_from_eviction(&just_used.task_id));
                }
                false
            })
            .expect("eviction should scan cache")
            .expect("quota should trigger eviction");

        assert_eq!(vec![evictable.task_id.clone()], summary.evicted_session_ids);
        assert!(summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&just_used.library_item_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&evictable.library_item_id)
                .is_none()
        );
        assert!(state.tasks.get_task(&just_used.task_id).is_ok());
        assert!(state.tasks.get_task(&evictable.task_id).is_err());
    }

    #[tokio::test]
    async fn hls_cache_quota_evicts_unprotected_sessions_for_oversized_projected_session() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 2,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 50,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let first =
            create_completed_hls_playback_task(&state, "BV1oversized-first", &upstream_url).await;
        let second =
            create_completed_hls_playback_task(&state, "BV1oversized-second", &upstream_url).await;

        let summary = state
            .enforce_hls_cache_quota("test", Vec::new(), session_size * 2)
            .expect("eviction should scan cache")
            .expect("quota should trigger and evict eligible sessions");

        assert_eq!(
            vec![first.task_id.clone(), second.task_id.clone()],
            summary.evicted_session_ids
        );
        assert_eq!(0, summary.finished_used_bytes);
        assert!(!summary.target_reached);
        assert_eq!(0, summary.target_used_bytes);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&first.library_item_id)
                .is_none()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&second.library_item_id)
                .is_none()
        );
        assert!(state.tasks.get_task(&first.task_id).is_err());
        assert!(state.tasks.get_task(&second.task_id).is_err());
    }

    #[tokio::test]
    async fn hls_cache_quota_cancellation_skips_pre_eviction() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 2,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 50,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let first =
            create_completed_hls_playback_task(&state, "BV1cancel-first", &upstream_url).await;
        let second =
            create_completed_hls_playback_task(&state, "BV1cancel-second", &upstream_url).await;

        let summary = state
            .enforce_hls_cache_quota_until_cancelled("test", Vec::new(), session_size, || true)
            .expect("cancelled eviction should not need cache mutation");

        assert!(summary.is_none());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&first.library_item_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&second.library_item_id)
                .is_some()
        );
        assert!(state.tasks.get_task(&first.task_id).is_ok());
        assert!(state.tasks.get_task(&second.task_id).is_ok());
    }

    #[tokio::test]
    async fn hls_cache_quota_preserves_orphan_cache_when_task_persistence_is_unavailable() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let task_state_path = root_path.join(".state").join("tasks.json");
        let session_size = fake_mp4().len() as u64;
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: task_state_path.clone(),
            hls_cache_max_bytes: session_size,
            hls_cache_high_watermark_percent: 90,
            hls_cache_low_watermark_percent: 0,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let cached =
            create_completed_hls_playback_task(&state, "BV1broken-state", &upstream_url).await;
        std::fs::write(&task_state_path, b"{ invalid task state")
            .expect("task state should be corruptible");

        let restored_state =
            AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        assert!(!restored_state.tasks.persistence_available());

        let summary = restored_state
            .enforce_hls_cache_quota("test", Vec::new(), 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger but skip orphan deletion");

        assert!(summary.evicted_session_ids.is_empty());
        assert!(!summary.target_reached);
        assert!(
            restored_state
                .hls_cache
                .get_completed_library_item(&cached.library_item_id)
                .is_some()
        );
    }

    #[tokio::test]
    async fn hls_cache_quota_skips_session_under_finalization_protection() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 0,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let protected =
            create_completed_hls_playback_task(&state, "BV1finalizing-cache", &upstream_url).await;

        let guard = state.protect_hls_cache_session_from_eviction(&protected.task_id);
        let protected_summary = state
            .enforce_hls_cache_quota("test", Vec::new(), 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger while protection is active");

        assert!(protected_summary.evicted_session_ids.is_empty());
        assert!(!protected_summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&protected.library_item_id)
                .is_some()
        );
        assert!(state.tasks.get_task(&protected.task_id).is_ok());

        drop(guard);
        let unprotected_summary = state
            .enforce_hls_cache_quota("test", Vec::new(), 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger after protection drops");

        assert_eq!(
            vec![protected.task_id.clone()],
            unprotected_summary.evicted_session_ids
        );
        assert!(unprotected_summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&protected.library_item_id)
                .is_none()
        );
        assert!(state.tasks.get_task(&protected.task_id).is_err());
    }

    #[tokio::test]
    async fn hls_cache_quota_evicts_unprotected_partial_session() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 2,
                hls_cache_high_watermark_percent: 50,
                hls_cache_low_watermark_percent: 0,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let protected =
            create_partial_hls_playback_task(&state, "BV1protected-partial", &upstream_url).await;
        let evictable =
            create_partial_hls_playback_task(&state, "BV1evictable-partial", &upstream_url).await;

        let summary = state
            .enforce_hls_cache_quota("test", [protected.task_id.clone()], 0)
            .expect("partial eviction should scan cache")
            .expect("partial usage should trigger eviction");

        assert_eq!(2 * session_size, summary.started_used_bytes);
        assert_eq!(session_size, summary.finished_used_bytes);
        assert_eq!(0, summary.target_used_bytes);
        assert_eq!(session_size, summary.evicted_bytes);
        assert_eq!(vec![evictable.task_id.clone()], summary.evicted_session_ids);
        assert!(!summary.target_reached);
        assert!(
            state
                .hls_cache
                .cached_resource(&protected.task_id, "video.m4s")
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .cached_resource(&evictable.task_id, "video.m4s")
                .is_none()
        );
        assert!(
            state
                .hls_cache
                .playback_session(&protected.task_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .playback_session(&evictable.task_id)
                .is_some()
        );
        let usage = state
            .hls_cache
            .usage_snapshot()
            .expect("usage snapshot should scan cache after partial eviction");
        assert_eq!(session_size, usage.used_bytes);
        assert!(state.hls_sessions.get(&evictable.task_id).is_some());
        assert!(state.tasks.get_task(&protected.task_id).is_ok());
        assert!(state.tasks.get_task(&evictable.task_id).is_ok());
    }

    #[tokio::test]
    async fn hls_cache_quota_evicts_stale_failed_progressive_cache() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 0,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let stale =
            create_completed_hls_playback_task(&state, "BV1stale-cache", &upstream_url).await;
        state
            .tasks
            .fail_completed_playback_task_after_cache_restore(
                &stale.task_id,
                "stale completed cache should be reclaimable".to_owned(),
            )
            .expect("completed playback task should become failed");

        let summary = state
            .enforce_hls_cache_quota("test", Vec::new(), 0)
            .expect("eviction should scan cache")
            .expect("quota should trigger for stale cache");

        assert_eq!(vec![stale.task_id.clone()], summary.evicted_session_ids);
        assert!(summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&stale.library_item_id)
                .is_none()
        );
        let task = state
            .tasks
            .get_task(&stale.task_id)
            .expect("failed task should remain for retention policy");
        assert_eq!(TaskState::Failed, task.state());
        assert!(state.hls_sessions.get(&stale.task_id).is_none());
    }

    #[tokio::test]
    async fn hls_cache_finalization_enforces_quota_after_unknown_projected_size() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 2,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 50,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let older =
            create_completed_hls_playback_task(&state, "BV1older-cache", &upstream_url).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (current_task_id, current_session, current_library_item_id) =
            create_playable_hls_playback_task(&state, "BV1current-cache", &upstream_url);

        assert_eq!(None, hls_session_declared_size_bytes(&current_session));

        run_hls_cache_finalization(
            state.clone(),
            current_task_id.clone(),
            current_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        )
        .await;

        let status = state
            .hls_cache_status()
            .expect("status should scan after finalization");
        let summary = status
            .last_eviction
            .expect("post-finalization quota should run");
        assert_eq!("after_hls_finalization", summary.reason);
        assert_eq!(2 * session_size, summary.started_used_bytes);
        assert_eq!(session_size, summary.finished_used_bytes);
        assert_eq!(session_size, summary.target_used_bytes);
        assert_eq!(vec![older.task_id.clone()], summary.evicted_session_ids);
        assert!(summary.target_reached);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&older.library_item_id)
                .is_none()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&current_library_item_id)
                .is_some()
        );
        assert!(state.tasks.get_task(&older.task_id).is_err());
        assert!(state.tasks.get_task(&current_task_id).is_ok());
    }

    #[tokio::test]
    async fn hls_cache_finalization_quota_projects_only_uncached_session_bytes() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: session_size * 4,
                hls_cache_high_watermark_percent: 90,
                hls_cache_low_watermark_percent: 50,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let older =
            create_completed_hls_playback_task(&state, "BV1projection-older", &upstream_url).await;
        let (current_task_id, mut current_session, current_library_item_id) =
            create_playable_hls_playback_task(&state, "BV1projection-current", &upstream_url);
        current_session.variant.video.request.size = Some(session_size);
        let mut audio = current_session.variant.video.clone();
        audio.id = "audio.m4s".to_owned();
        audio.request.kind = BilibiliMediaRequestKind::Audio;
        audio.request.codecs = Some("mp4a.40.2".to_owned());
        audio.request.size = Some(session_size);
        audio.request.cache_key.media_kind = BilibiliMediaRequestKind::Audio;
        audio.request.cache_key.codecs = Some("mp4a.40.2".to_owned());
        current_session.variant.audio = Some(audio);
        state.hls_sessions.insert(current_session.clone());

        let cache_for_preempt = state.hls_cache.clone();
        let task_id_for_preempt = current_task_id.clone();
        let error = state
            .hls_cache
            .cache_session_resources_with_control(
                &state.hls_upstream_client,
                &current_session,
                move || {
                    if cache_for_preempt
                        .cached_resource(&task_id_for_preempt, "video.m4s")
                        .is_some()
                    {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                },
                |_| {},
            )
            .await
            .expect_err("preempted fill should leave a partial current session");
        assert!(matches!(error, crate::hls_cache::HlsCacheError::Preempted));

        let partial_status = state
            .hls_cache_status()
            .expect("partial status should scan cache");
        assert_eq!(2 * session_size, partial_status.usage.used_bytes);
        assert!(partial_status.last_eviction.is_none());

        run_hls_cache_finalization(
            state.clone(),
            current_task_id.clone(),
            current_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        )
        .await;

        let status = state
            .hls_cache_status()
            .expect("status should scan after finalization");
        assert_eq!(3 * session_size, status.usage.used_bytes);
        assert!(status.last_eviction.is_none());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&older.library_item_id)
                .is_some()
        );
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&current_library_item_id)
                .is_some()
        );
        assert!(state.tasks.get_task(&older.task_id).is_ok());
        assert!(state.tasks.get_task(&current_task_id).is_ok());
    }

    #[tokio::test]
    async fn disabled_hls_cache_quota_does_not_evict_completed_sessions() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                hls_cache_max_bytes: 0,
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let cached =
            create_completed_hls_playback_task(&state, "BV1disabled-cache", &upstream_url).await;

        let summary = state
            .enforce_hls_cache_quota("test", Vec::new(), u64::MAX)
            .expect("disabled eviction should not need cache scan");
        let status = state
            .hls_cache_status()
            .expect("status should scan disabled cache");

        assert!(summary.is_none());
        assert!(!status.policy.eviction_enabled());
        assert!(status.last_eviction.is_none());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&cached.library_item_id)
                .is_some()
        );
        assert!(state.tasks.get_task(&cached.task_id).is_ok());
    }

    #[tokio::test]
    async fn completed_hls_items_are_hidden_when_cache_playback_is_unsupported() {
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
        let mut state = AppState::new_with_playback_planner(options, Arc::new(planner));
        let task_service = TaskGrpcService::new(state.clone());

        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1offline".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
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

        let completed = wait_for_task_state(&state.tasks, &created.id, TaskState::Completed).await;
        let expected_item_id = format!("bilibili.hls.{}", completed.id);
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&expected_item_id)
                .is_some()
        );

        state.completed_hls_cache_playback_supported = false;
        let library_service = LibraryGrpcService::new(state.clone());

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
            .expect("library list should succeed")
            .into_inner();
        assert!(library.items.is_empty());
        assert!(
            state
                .get_completed_hls_library_item(&expected_item_id)
                .is_none()
        );
        assert!(
            state
                .create_completed_hls_playback_source(
                    &expected_item_id,
                    "h264",
                    "http://media.example.test:8080/hls/blocked/master.m3u8".to_owned(),
                )
                .is_none()
        );

        let item_error = library_service
            .get_library_item(Request::new(GetLibraryItemRequest {
                id: expected_item_id.clone(),
            }))
            .await
            .expect_err("completed HLS item should be hidden");
        assert_eq!(tonic::Code::NotFound, item_error.code());

        let source_error = library_service
            .get_playback_source(Request::new(GetPlaybackSourceRequest {
                item_id: expected_item_id,
                variant_id: "h264".to_owned(),
            }))
            .await
            .expect_err("completed HLS playback source should be hidden");
        assert_eq!(tonic::Code::NotFound, source_error.code());
    }

    #[tokio::test]
    async fn completed_child_hls_sessions_are_hidden_when_cache_playback_is_unsupported() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let mut state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1hidden-child", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let primary_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("child playback metadata should map");
        state
            .hls_cache
            .save_session(&primary_metadata.hls_session)
            .expect("primary HLS session should persist");
        state
            .hls_cache
            .save_session(&child_metadata.hls_session)
            .expect("child HLS session should persist");
        state
            .hls_sessions
            .insert(primary_metadata.hls_session.clone());
        state
            .hls_sessions
            .insert(child_metadata.hls_session.clone());

        let primary_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                "Multi Result".to_owned(),
                "All results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: creation.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session.clone()),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session.clone()),
                    },
                ],
            )
            .expect("multi-result playback task should become playable");

        let primary_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &primary_metadata.hls_session)
            .await
            .expect("primary HLS resources should cache");
        state
            .tasks
            .complete_playback_hls_session_cached(
                &creation.task.id,
                &creation.task.id,
                primary_library_item_id,
            )
            .expect("primary HLS session should become completed");

        state.completed_hls_cache_playback_supported = false;
        state.remove_hls_playback_session(&child_session_id);

        assert!(
            state
                .hls_cache
                .playback_session(&child_session_id)
                .is_some()
        );
        assert!(
            state
                .hls_playback_session_for_serving(&child_session_id)
                .is_none()
        );
    }

    #[tokio::test]
    async fn playback_task_stays_playable_when_cache_playback_is_unsupported() {
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
        let mut state = AppState::new_with_playback_planner(options, Arc::new(planner));
        state.completed_hls_cache_playback_supported = false;
        let task_service = TaskGrpcService::new(state.clone());

        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1runtime".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: None,
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
        tokio::time::sleep(Duration::from_millis(150)).await;
        let still_playable = state
            .tasks
            .get_task(&created.id)
            .expect("playback task should remain readable");
        assert_eq!(TaskState::Playable, still_playable.state());
        assert!(still_playable.library_item_id.is_empty());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&format!("bilibili.hls.{}", created.id))
                .is_none()
        );
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
                selection_id: String::new(),
                selection: None,
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
    async fn app_state_removes_incomplete_hls_session_after_interrupted_result_planning() {
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
            .create_bilibili_playback_task("BV1interrupted-result-planning", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url("https://example.test/video.m4s"),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist child HLS session");
        state
            .tasks
            .update_playback_results(
                &creation.task.id,
                Some("Partially planned playback".to_owned()),
                "Planning selected Bilibili playback results.".to_owned(),
                0.5,
                vec![BilibiliTaskResultItem {
                    id: child_session_id.clone(),
                    selection_id: "page:2".to_owned(),
                    title: "Part 2".to_owned(),
                    subtitle: String::new(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-2".to_owned(),
                    index: 2,
                    state: TaskState::Playable.into(),
                    message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                    library_item_id: String::new(),
                    playback_source: Some(PlaybackSource {
                        item_id: child_session_id.clone(),
                        variant_id: metadata.playback_session.selected_variant_id.clone(),
                        protocol: PlaybackProtocol::Hls.into(),
                        uri: format!(
                            "http://media.example.test:8080/hls/{child_session_id}/master.m3u8"
                        ),
                        expires_at: None,
                    }),
                    playback_session: Some(metadata.playback_session),
                }],
            )
            .expect("partial playback results should persist");
        let child_session_dir = root_path
            .join(".tvos-net-player")
            .join("hls")
            .join(&child_session_id);
        assert!(child_session_dir.exists());

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("interrupted task should restore as failed");

        assert_eq!(TaskState::Failed, restored_task.state());
        assert!(restored.hls_sessions.get(&child_session_id).is_none());
        assert!(!child_session_dir.exists());
    }

    #[tokio::test]
    async fn app_state_rejects_registered_unfinished_result_hls_session_for_serving() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path,
                task_state_path: temp.path().join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1unfinished-result-session", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url("https://example.test/video.m4s"),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist child HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        state
            .tasks
            .update_playback_results(
                &creation.task.id,
                Some("Partially planned playback".to_owned()),
                "Planning selected Bilibili playback results.".to_owned(),
                0.5,
                vec![BilibiliTaskResultItem {
                    id: child_session_id.clone(),
                    selection_id: "page:2".to_owned(),
                    title: "Part 2".to_owned(),
                    subtitle: String::new(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-2".to_owned(),
                    index: 2,
                    state: TaskState::Playable.into(),
                    message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                    library_item_id: String::new(),
                    playback_source: Some(PlaybackSource {
                        item_id: child_session_id.clone(),
                        variant_id: metadata.playback_session.selected_variant_id.clone(),
                        protocol: PlaybackProtocol::Hls.into(),
                        uri: format!(
                            "http://media.example.test:8080/hls/{child_session_id}/master.m3u8"
                        ),
                        expires_at: None,
                    }),
                    playback_session: Some(metadata.playback_session),
                }],
            )
            .expect("partial playback results should persist");

        assert!(state.hls_sessions.get(&child_session_id).is_some());
        assert!(
            state
                .hls_playback_session_for_serving(&child_session_id)
                .is_none()
        );
        assert!(state.hls_sessions.get(&child_session_id).is_none());
        let task = state
            .tasks
            .get_task(&creation.task.id)
            .expect("unfinished task should remain readable");
        assert_eq!(TaskState::Preparing, task.state());
    }

    #[tokio::test]
    async fn app_state_preserves_hls_tasks_when_cache_scan_fails() {
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
            .create_bilibili_playback_task("BV1scan-error", None, None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url("https://example.test/video.m4s"),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                PlaybackSource {
                    item_id: creation.task.id.clone(),
                    variant_id: metadata.playback_session.selected_variant_id.clone(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: format!(
                        "http://media.example.test:8080/hls/{}/master.m3u8",
                        creation.task.id
                    ),
                    expires_at: None,
                },
                metadata.playback_session,
            )
            .expect("task should become playable");
        let hls_root = root_path.join(".tvos-net-player").join("hls");
        fs::remove_dir_all(&hls_root).expect("HLS cache root should be removable");
        fs::write(&hls_root, b"not a directory").expect("HLS cache root probe file should save");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should remain persisted when cache scan fails");

        assert_eq!(TaskState::Playable, restored_task.state());
        assert!(restored_task.playback_source.is_some());
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());
    }

    #[tokio::test]
    async fn app_state_preserves_hls_tasks_when_cache_root_is_missing() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("external-cache-root");
        fs::create_dir_all(&root_path).expect("cache root should be created");
        let root_path = root_path
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()).join("external-cache-root"));
        let task_state_path = temp.path().join("tasks.json");
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
            .create_bilibili_playback_task("BV1missing-root", None, None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url("https://example.test/video.m4s"),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                PlaybackSource {
                    item_id: creation.task.id.clone(),
                    variant_id: metadata.playback_session.selected_variant_id.clone(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: format!(
                        "http://media.example.test:8080/hls/{}/master.m3u8",
                        creation.task.id
                    ),
                    expires_at: None,
                },
                metadata.playback_session,
            )
            .expect("task should become playable");
        fs::remove_dir_all(&root_path).expect("cache root should become unavailable");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should remain persisted when cache root is missing");

        assert_eq!(TaskState::Playable, restored_task.state());
        assert!(restored_task.playback_source.is_some());
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_hls_source_registers_session_after_cache_scan_recovers() {
        use std::os::unix::fs::PermissionsExt;

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
            .create_bilibili_playback_task("BV1scan-recover", None, None)
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
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                PlaybackSource {
                    item_id: creation.task.id.clone(),
                    variant_id: metadata.playback_session.selected_variant_id.clone(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: format!(
                        "http://media.example.test:8080/hls/{}/master.m3u8",
                        creation.task.id
                    ),
                    expires_at: None,
                },
                metadata.playback_session,
            )
            .expect("task should become playable");
        let library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &metadata.hls_session)
            .await
            .expect("HLS resources should cache");
        state
            .tasks
            .complete_playback_cached(&creation.task.id, library_item_id.clone())
            .expect("task should become completed");

        let hls_root = root_path.join(".tvos-net-player").join("hls");
        let mut unreadable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should exist")
            .permissions();
        unreadable_permissions.set_mode(0o000);
        fs::set_permissions(&hls_root, unreadable_permissions)
            .expect("HLS cache root should become unreadable");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("completed task should remain persisted when cache scan fails");
        assert_eq!(TaskState::Completed, restored_task.state());
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());

        let mut readable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should remain present")
            .permissions();
        readable_permissions.set_mode(0o700);
        fs::set_permissions(&hls_root, readable_permissions)
            .expect("HLS cache root should become readable again");

        let direct_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(restored.clone())),
            AxumPath(creation.task.id.clone()),
        )
        .await;
        assert_eq!(StatusCode::OK, direct_master.status());
        assert!(restored.hls_sessions.get(&creation.task.id).is_some());
        restored.remove_hls_playback_session(&creation.task.id);

        let library_service = LibraryGrpcService::new(restored.clone());
        let item = library_service
            .get_library_item(Request::new(GetLibraryItemRequest {
                id: library_item_id.clone(),
            }))
            .await
            .expect("completed HLS item should load after cache root recovers")
            .into_inner();
        assert_eq!(library_item_id, item.id);

        let source = library_service
            .get_playback_source(Request::new(GetPlaybackSourceRequest {
                item_id: library_item_id,
                variant_id: metadata.hls_session.variant.id.clone(),
            }))
            .await
            .expect("completed HLS source should load after cache root recovers")
            .into_inner();
        assert_eq!(PlaybackProtocol::Hls as i32, source.protocol);
        assert!(restored.hls_sessions.get(&creation.task.id).is_some());
    }

    #[tokio::test]
    async fn app_state_fails_completed_hls_task_with_stale_library_item_id() {
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
            .create_bilibili_playback_task("BV1stale-library-item", None, None)
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
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                PlaybackSource {
                    item_id: creation.task.id.clone(),
                    variant_id: metadata.playback_session.selected_variant_id.clone(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: format!(
                        "http://media.example.test:8080/hls/{}/master.m3u8",
                        creation.task.id
                    ),
                    expires_at: None,
                },
                metadata.playback_session,
            )
            .expect("task should become playable");
        let library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &metadata.hls_session)
            .await
            .expect("HLS resources should cache");
        state
            .tasks
            .complete_playback_cached(&creation.task.id, library_item_id)
            .expect("task should become completed");

        let mut snapshot: serde_json::Value = serde_json::from_slice(
            &fs::read(&task_state_path).expect("task snapshot should be readable"),
        )
        .expect("task snapshot should be valid JSON");
        let task = snapshot["tasks"]
            .as_array_mut()
            .expect("task snapshot should contain tasks")
            .iter_mut()
            .find(|task| task["id"].as_str() == Some(creation.task.id.as_str()))
            .expect("completed task should be persisted");
        task["library_item_id"] = serde_json::Value::String("bilibili.hls.stale".to_owned());
        fs::write(
            &task_state_path,
            serde_json::to_vec_pretty(&snapshot).expect("task snapshot should serialize"),
        )
        .expect("task snapshot should be overwritten");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let failed = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("stale completed task should remain readable");
        assert_eq!(TaskState::Failed, failed.state());
        assert!(failed.library_item_id.is_empty());
        assert!(failed.playback_source.is_none());
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_completed_hls_task_fails_after_cache_scan_recovers() {
        use std::os::unix::fs::PermissionsExt;

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
            .create_bilibili_playback_task("BV1scan-stale", None, None)
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
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                PlaybackSource {
                    item_id: creation.task.id.clone(),
                    variant_id: metadata.playback_session.selected_variant_id.clone(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: format!(
                        "http://media.example.test:8080/hls/{}/master.m3u8",
                        creation.task.id
                    ),
                    expires_at: None,
                },
                metadata.playback_session,
            )
            .expect("task should become playable");
        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &metadata.hls_session)
            .await
            .expect("HLS resources should cache");
        state
            .tasks
            .complete_playback_cached(
                &creation.task.id,
                HlsCacheStore::completed_library_item_id(&creation.task.id),
            )
            .expect("task should become completed");

        let mut snapshot: serde_json::Value = serde_json::from_slice(
            &fs::read(&task_state_path).expect("task snapshot should be readable"),
        )
        .expect("task snapshot should be valid JSON");
        let task = snapshot["tasks"]
            .as_array_mut()
            .expect("task snapshot should contain tasks")
            .iter_mut()
            .find(|task| task["id"].as_str() == Some(creation.task.id.as_str()))
            .expect("completed task should be persisted");
        task["library_item_id"] = serde_json::Value::String("bilibili.hls.stale".to_owned());
        fs::write(
            &task_state_path,
            serde_json::to_vec_pretty(&snapshot).expect("task snapshot should serialize"),
        )
        .expect("task snapshot should be overwritten");

        let hls_root = root_path.join(".tvos-net-player").join("hls");
        let mut unreadable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should exist")
            .permissions();
        unreadable_permissions.set_mode(0o000);
        fs::set_permissions(&hls_root, unreadable_permissions)
            .expect("HLS cache root should become unreadable");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let preserved = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("stale completed task should remain readable until cache can be checked");
        assert_eq!(TaskState::Completed, preserved.state());
        assert_eq!("bilibili.hls.stale", preserved.library_item_id);

        let mut readable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should remain present")
            .permissions();
        readable_permissions.set_mode(0o700);
        fs::set_permissions(&hls_root, readable_permissions)
            .expect("HLS cache root should become readable again");

        let direct_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(restored.clone())),
            AxumPath(creation.task.id.clone()),
        )
        .await;
        assert_eq!(StatusCode::NOT_FOUND, direct_master.status());
        let failed = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("stale task should remain readable after failure");
        assert_eq!(TaskState::Failed, failed.state());
        assert!(failed.library_item_id.is_empty());
        assert!(failed.playback_source.is_none());
        assert!(failed.playback_session.is_none());
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());
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
            .create_bilibili_playback_task("BV1offline", None, None)
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
    async fn app_state_restore_shortcut_enforces_quota_after_completed_hls_cache_restart() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            hls_cache_max_bytes: session_size * 2,
            hls_cache_high_watermark_percent: 90,
            hls_cache_low_watermark_percent: 50,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let older =
            create_completed_hls_playback_task(&state, "BV1older-cache", &upstream_url).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (current_task_id, current_session, current_library_item_id) =
            create_playable_hls_playback_task(&state, "BV1current-cache", &upstream_url);
        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &current_session)
            .await
            .expect("HLS resources should already be complete before restart");
        let playable = state
            .tasks
            .get_task(&current_task_id)
            .expect("current task should remain persisted");
        assert_eq!(TaskState::Playable, playable.state());

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let completed =
            wait_for_task_state(&restored.tasks, &current_task_id, TaskState::Completed).await;

        assert_eq!(current_library_item_id, completed.library_item_id);
        let status = restored
            .hls_cache_status()
            .expect("status should scan after startup finalization");
        let summary = status
            .last_eviction
            .expect("startup finalization should run post-cache quota");
        assert_eq!("after_hls_finalization", summary.reason);
        assert_eq!(2 * session_size, summary.started_used_bytes);
        assert_eq!(session_size, summary.finished_used_bytes);
        assert_eq!(session_size, summary.target_used_bytes);
        assert_eq!(vec![older.task_id.clone()], summary.evicted_session_ids);
        assert!(summary.target_reached);
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&older.library_item_id)
                .is_none()
        );
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&current_library_item_id)
                .is_some()
        );
        assert!(restored.tasks.get_task(&older.task_id).is_err());
        assert!(restored.tasks.get_task(&current_task_id).is_ok());
    }

    #[tokio::test]
    async fn app_state_restore_shortcut_enforces_quota_after_completed_secondary_hls_cache_restart()
    {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let session_size = fake_mp4().len() as u64;
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            hls_cache_max_bytes: session_size * 2,
            hls_cache_high_watermark_percent: 90,
            hls_cache_low_watermark_percent: 50,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let older =
            create_completed_hls_playback_task(&state, "BV1older-secondary-cache", &upstream_url)
                .await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1current-secondary-cache", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let primary_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("secondary playback metadata should map");
        state
            .hls_cache
            .save_session(&primary_metadata.hls_session)
            .expect("primary session should persist");
        state
            .hls_cache
            .save_session(&child_metadata.hls_session)
            .expect("secondary session should persist");
        state
            .hls_sessions
            .insert(primary_metadata.hls_session.clone());
        state
            .hls_sessions
            .insert(child_metadata.hls_session.clone());
        let primary_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                "Playable".to_owned(),
                "All selected Bilibili playback results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: creation.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session),
                    },
                ],
            )
            .expect("task should become playable");
        let child_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &child_metadata.hls_session)
            .await
            .expect("secondary HLS resources should already be complete before restart");
        let playable = state
            .tasks
            .complete_playback_hls_session_cached(
                &creation.task.id,
                &child_session_id,
                child_library_item_id.clone(),
            )
            .expect("secondary result should become completed while parent stays playable");
        assert_eq!(TaskState::Playable, playable.state());

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("current parent task should remain persisted");

        assert_eq!(TaskState::Playable, restored_task.state());
        assert_eq!(2, restored_task.result_items.len());
        assert_eq!(
            i32::from(TaskState::Completed),
            restored_task.result_items[1].state
        );
        assert_eq!(
            child_library_item_id,
            restored_task.result_items[1].library_item_id
        );
        let status = restored
            .hls_cache_status()
            .expect("status should scan after startup finalization");
        let summary = status
            .last_eviction
            .expect("startup secondary finalization should run post-cache quota");
        assert_eq!("after_hls_finalization", summary.reason);
        assert_eq!(2 * session_size, summary.started_used_bytes);
        assert_eq!(session_size, summary.finished_used_bytes);
        assert_eq!(session_size, summary.target_used_bytes);
        assert_eq!(vec![older.task_id.clone()], summary.evicted_session_ids);
        assert!(summary.target_reached);
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&older.library_item_id)
                .is_none()
        );
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&child_library_item_id)
                .is_some()
        );
        assert!(restored.tasks.get_task(&older.task_id).is_err());
    }

    #[tokio::test]
    async fn app_state_restore_skips_failed_primary_cache_fill_retry_when_online_playable() {
        let (primary_upstream_url, _primary_upstream_task) = start_failing_mp4_upstream().await;
        let (child_upstream_url, _child_upstream_task) = start_mp4_upstream().await;
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
            .create_bilibili_playback_task("BV1primary-cache-fill-failed", None, None)
            .expect("playback task should be created");
        let task_id = creation.task.id.clone();
        let child_session_id = format!("{task_id}-result-2");
        let primary_metadata = playback_task_metadata(
            &task_id,
            sample_playback_plan_with_video_url(&primary_upstream_url),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&child_upstream_url),
        )
        .expect("secondary playback metadata should map");
        state
            .hls_cache
            .save_session(&primary_metadata.hls_session)
            .expect("primary session should persist");
        state
            .hls_cache
            .save_session(&child_metadata.hls_session)
            .expect("secondary session should persist");
        state
            .hls_sessions
            .insert(primary_metadata.hls_session.clone());
        state
            .hls_sessions
            .insert(child_metadata.hls_session.clone());
        let primary_source = PlaybackSource {
            item_id: task_id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{task_id}/master.m3u8"),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &task_id,
                "Playable".to_owned(),
                "All selected Bilibili playback results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: task_id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session.clone()),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session.clone()),
                    },
                ],
            )
            .expect("task should become playable");
        let degraded = state
            .tasks
            .fail_hls_cache_fill_for_playback_session(
                &task_id,
                &task_id,
                "Playable online; offline cache fill failed: upstream returned 503".to_owned(),
            )
            .expect("primary cache fill failure should publish")
            .expect("primary cache fill failure should update task");

        assert_eq!(TaskState::Playable, degraded.state());
        assert_eq!(i32::from(TaskState::Failed), degraded.result_items[0].state);
        assert!(
            state
                .tasks
                .hls_session_has_online_playback_after_cache_fill_failure(&task_id, &task_id)
        );
        let child_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &child_metadata.hls_session)
            .await
            .expect("secondary HLS resources should cache before restart");
        let updated = state
            .tasks
            .complete_playback_hls_session_cached_with_metadata(
                &task_id,
                &child_session_id,
                child_library_item_id,
                playback_session_from_hls_cache_session(&child_metadata.hls_session),
            )
            .expect("secondary result should become completed");

        assert_eq!(
            "Playable online; selected Bilibili playback results are cached offline.",
            updated.message
        );
        assert_eq!(i32::from(TaskState::Failed), updated.result_items[0].state);
        assert_eq!(
            i32::from(TaskState::Completed),
            updated.result_items[1].state
        );
        assert!(
            state
                .tasks
                .hls_session_has_online_playback_after_cache_fill_failure(&task_id, &task_id),
            "primary degraded marker should survive secondary task message updates"
        );

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&task_id)
            .expect("playable primary task should restore");

        assert!(
            !restored.hls_fill_scheduler.worker_started_for_tests(),
            "failed-but-playable primary cache fill should not be retried during restore"
        );
        assert_eq!(TaskState::Playable, restored_task.state());
        assert_eq!(
            "Playable online; selected Bilibili playback results are cached offline.",
            restored_task.message
        );
        assert_eq!(
            i32::from(TaskState::Failed),
            restored_task.result_items[0].state
        );
        assert_eq!(
            "Playable online; offline cache fill failed: upstream returned 503",
            restored_task.result_items[0].message
        );
        assert!(restored_task.playback_source.is_some());
        assert!(restored_task.playback_session.is_some());
        assert!(
            restored
                .tasks
                .is_hls_session_playable_for_task(&task_id, &task_id)
        );
    }

    #[tokio::test]
    async fn app_state_restore_skips_failed_secondary_cache_fill_retry_when_online_playable() {
        let (primary_upstream_url, _primary_upstream_task) = start_mp4_upstream().await;
        let (child_upstream_url, _child_upstream_task) = start_failing_mp4_upstream().await;
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
            .create_bilibili_playback_task("BV1secondary-cache-fill-failed", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let primary_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&primary_upstream_url),
        )
        .expect("primary playback metadata should map");
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&child_upstream_url),
        )
        .expect("secondary playback metadata should map");
        state
            .hls_cache
            .save_session(&primary_metadata.hls_session)
            .expect("primary session should persist");
        state
            .hls_cache
            .save_session(&child_metadata.hls_session)
            .expect("secondary session should persist");
        state
            .hls_sessions
            .insert(primary_metadata.hls_session.clone());
        state
            .hls_sessions
            .insert(child_metadata.hls_session.clone());
        let primary_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: primary_metadata
                .playback_session
                .selected_variant_id
                .clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://stale.example.test/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                "Playable".to_owned(),
                "All selected Bilibili playback results are playable.".to_owned(),
                primary_source.clone(),
                primary_metadata.playback_session.clone(),
                vec![
                    BilibiliTaskResultItem {
                        id: creation.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(primary_source),
                        playback_session: Some(primary_metadata.playback_session.clone()),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(child_source),
                        playback_session: Some(child_metadata.playback_session.clone()),
                    },
                ],
            )
            .expect("task should become playable");
        let primary_library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &primary_metadata.hls_session)
            .await
            .expect("primary HLS resources should cache before restart");
        state
            .tasks
            .complete_playback_hls_session_cached_with_metadata(
                &creation.task.id,
                &creation.task.id,
                primary_library_item_id,
                playback_session_from_hls_cache_session(&primary_metadata.hls_session),
            )
            .expect("primary result should become completed");
        let degraded = state
            .tasks
            .fail_hls_cache_fill_for_playback_session(
                &creation.task.id,
                &child_session_id,
                "Playable online; offline cache fill failed: upstream returned 503".to_owned(),
            )
            .expect("secondary cache fill failure should publish")
            .expect("secondary cache fill failure should update task");

        assert_eq!(TaskState::Completed, degraded.state());
        assert_eq!(i32::from(TaskState::Failed), degraded.result_items[1].state);
        assert!(
            state
                .tasks
                .hls_session_has_online_playback_after_cache_fill_failure(
                    &creation.task.id,
                    &child_session_id,
                )
        );

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("completed parent task should restore");

        assert!(
            !restored.hls_fill_scheduler.worker_started_for_tests(),
            "failed-but-playable secondary cache fill should not be retried during restore"
        );
        assert_eq!(TaskState::Completed, restored_task.state());
        assert_eq!(
            i32::from(TaskState::Failed),
            restored_task.result_items[1].state
        );
        let restored_child_source = restored_task.result_items[1]
            .playback_source
            .as_ref()
            .expect("failed secondary result should keep refreshed online playback source");
        assert_eq!(
            format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            restored_child_source.uri
        );
        assert!(restored_task.result_items[1].playback_session.is_some());
        assert!(
            restored
                .tasks
                .is_playback_result_session_playable(&child_session_id, true)
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
            .create_bilibili_playback_task("BV1offline", None, None)
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
            !root_path
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
            .create_bilibili_playback_task("BV1offline", None, None)
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
    async fn app_state_preserves_restored_hls_uri_without_public_media_base() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            media_listen_url: "http://0.0.0.0:8080".to_owned(),
            public_media_base_uri: None,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1offline", None, None)
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
        let lan_uri = format!("http://10.0.0.5:8080/hls/{}/master.m3u8", creation.task.id);
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                PlaybackSource {
                    item_id: creation.task.id.clone(),
                    variant_id: metadata.playback_session.selected_variant_id.clone(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: lan_uri.clone(),
                    expires_at: None,
                },
                metadata.playback_session,
            )
            .expect("task should become playable");
        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &metadata.hls_session)
            .await
            .expect("HLS resources should cache");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let completed = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should restore");

        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(
            lan_uri,
            completed
                .playback_source
                .as_ref()
                .expect("restored task should keep playback source")
                .uri
        );
    }

    #[tokio::test]
    async fn app_state_refreshes_restored_secondary_result_hls_uri() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let initial_options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://old-media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state = AppState::new_with_playback_planner(
            initial_options.clone(),
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1restore-secondary", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist secondary HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let stale_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://old-media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        let result_items = vec![
            BilibiliTaskResultItem {
                id: creation.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Failed.into(),
                message: "page 1 planning failed".to_owned(),
                library_item_id: String::new(),
                playback_source: None,
                playback_session: None,
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                library_item_id: String::new(),
                playback_source: Some(stale_source.clone()),
                playback_session: Some(metadata.playback_session.clone()),
            },
        ];
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                metadata.title,
                "1/2 Bilibili playback result(s) are playable.".to_owned(),
                stale_source,
                metadata.playback_session,
                result_items,
            )
            .expect("task should become partially playable");
        let restored_options = CacheServerOptions {
            public_media_base_uri: Some("http://restored-media.example.test:9090".to_owned()),
            ..initial_options
        };

        let restored =
            AppState::new_with_playback_planner(restored_options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should restore");
        let expected_uri =
            format!("http://restored-media.example.test:9090/hls/{child_session_id}/master.m3u8");

        assert_eq!(TaskState::Playable, restored_task.state());
        assert_eq!(
            creation.task.id,
            restored_task
                .playback_source
                .as_ref()
                .expect("primary source should refresh")
                .item_id
        );
        assert_eq!(
            expected_uri,
            restored_task
                .playback_source
                .as_ref()
                .expect("primary source should refresh")
                .uri
        );
        assert_eq!(2, restored_task.result_items.len());
        assert_eq!(
            child_session_id,
            restored_task.result_items[1]
                .playback_source
                .as_ref()
                .expect("secondary result source should refresh")
                .item_id
        );
        assert_eq!(
            expected_uri,
            restored_task.result_items[1]
                .playback_source
                .as_ref()
                .expect("secondary result source should refresh")
                .uri
        );
        assert!(restored.hls_sessions.get(&child_session_id).is_some());
    }

    #[tokio::test]
    async fn app_state_lazy_restored_primary_hls_refreshes_uri_after_cache_scan_recovers() {
        use std::os::unix::fs::PermissionsExt;

        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let initial_options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://old-media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state = AppState::new_with_playback_planner(
            initial_options.clone(),
            Arc::new(EmptyPlaybackPlanner),
        );
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1lazy-primary", None, None)
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
        let stale_source = PlaybackSource {
            item_id: creation.task.id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!(
                "http://old-media.example.test:8080/hls/{}/master.m3u8",
                creation.task.id
            ),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_playable(
                &creation.task.id,
                metadata.title,
                stale_source.clone(),
                metadata.playback_session,
            )
            .expect("task should become playable");

        let hls_root = root_path.join(".tvos-net-player").join("hls");
        let mut unreadable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should exist")
            .permissions();
        unreadable_permissions.set_mode(0o000);
        fs::set_permissions(&hls_root, unreadable_permissions)
            .expect("HLS cache root should become unreadable");

        let restored_options = CacheServerOptions {
            public_media_base_uri: Some("http://restored-media.example.test:9090".to_owned()),
            ..initial_options
        };
        let restored =
            AppState::new_with_playback_planner(restored_options, Arc::new(EmptyPlaybackPlanner));
        let preserved = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should stay persisted while cache scan fails");
        assert_eq!(TaskState::Playable, preserved.state());
        assert_eq!(
            stale_source.uri,
            preserved
                .playback_source
                .as_ref()
                .expect("playback source should stay stale before lazy recovery")
                .uri
        );
        assert!(restored.hls_sessions.get(&creation.task.id).is_none());

        let mut readable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should remain present")
            .permissions();
        readable_permissions.set_mode(0o700);
        fs::set_permissions(&hls_root, readable_permissions)
            .expect("HLS cache root should become readable again");

        let direct_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(restored.clone())),
            AxumPath(creation.task.id.clone()),
        )
        .await;

        let expected_uri = format!(
            "http://restored-media.example.test:9090/hls/{}/master.m3u8",
            creation.task.id
        );
        assert_eq!(StatusCode::OK, direct_master.status());
        assert!(restored.hls_sessions.get(&creation.task.id).is_some());
        let refreshed = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should still be available");
        assert_eq!(
            expected_uri,
            refreshed
                .playback_source
                .as_ref()
                .expect("playback source should refresh after lazy recovery")
                .uri
        );
    }

    #[tokio::test]
    async fn app_state_lazy_missing_child_primary_hls_fails_stale_playable_task() {
        use std::os::unix::fs::PermissionsExt;

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
            .create_bilibili_playback_task("BV1lazy-missing-child-primary", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("playback metadata should map");
        state
            .hls_cache
            .save_session(&metadata.hls_session)
            .expect("planning should persist child HLS session");
        state.hls_sessions.insert(metadata.hls_session.clone());
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        let result_items = vec![
            BilibiliTaskResultItem {
                id: creation.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Failed.into(),
                message: "page 1 planning failed".to_owned(),
                library_item_id: String::new(),
                playback_source: None,
                playback_session: None,
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                library_item_id: String::new(),
                playback_source: Some(child_source.clone()),
                playback_session: Some(metadata.playback_session.clone()),
            },
        ];
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                metadata.title,
                "1/2 Bilibili playback result(s) are playable.".to_owned(),
                child_source,
                metadata.playback_session,
                result_items,
            )
            .expect("task should become partially playable");

        let hls_root = root_path.join(".tvos-net-player").join("hls");
        let child_session_dir = hls_root.join(&child_session_id);
        let mut unreadable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should exist")
            .permissions();
        unreadable_permissions.set_mode(0o000);
        fs::set_permissions(&hls_root, unreadable_permissions)
            .expect("HLS cache root should become unreadable");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let preserved = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playable task should stay persisted while cache scan fails");
        assert_eq!(TaskState::Playable, preserved.state());
        assert!(restored.hls_sessions.get(&child_session_id).is_none());

        let mut readable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should remain present")
            .permissions();
        readable_permissions.set_mode(0o700);
        fs::set_permissions(&hls_root, readable_permissions)
            .expect("HLS cache root should become readable again");
        fs::remove_dir_all(&child_session_dir).expect("child HLS session should be removable");

        let direct_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(restored.clone())),
            AxumPath(child_session_id.clone()),
        )
        .await;

        assert_eq!(StatusCode::NOT_FOUND, direct_master.status());
        let failed = restored
            .tasks
            .get_task(&creation.task.id)
            .expect("playback task should remain readable after lazy cleanup");
        assert_eq!(TaskState::Failed, failed.state());
        assert!(failed.playback_source.is_none());
        assert!(failed.playback_session.is_none());
        assert!(
            failed
                .result_items
                .iter()
                .all(|item| item.playback_source.is_none() && item.playback_session.is_none())
        );
    }

    #[tokio::test]
    async fn app_state_rejects_stale_parent_hls_session_when_child_result_is_primary() {
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
            .create_bilibili_playback_task("BV1stale-parent-session", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", creation.task.id);
        let child_metadata = playback_task_metadata(
            &child_session_id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("child playback metadata should map");
        let stale_parent_metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(&upstream_url),
        )
        .expect("parent playback metadata should map");
        state
            .hls_cache
            .save_session(&stale_parent_metadata.hls_session)
            .expect("stale parent HLS session should persist");
        let child_source = PlaybackSource {
            item_id: child_session_id.clone(),
            variant_id: child_metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{child_session_id}/master.m3u8"),
            expires_at: None,
        };
        let result_items = vec![
            BilibiliTaskResultItem {
                id: creation.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Failed.into(),
                message: "page 1 planning failed".to_owned(),
                library_item_id: String::new(),
                playback_source: None,
                playback_session: None,
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: BILIBILI_RESULT_PLAYABLE_MESSAGE.to_owned(),
                library_item_id: String::new(),
                playback_source: Some(child_source.clone()),
                playback_session: Some(child_metadata.playback_session.clone()),
            },
        ];
        state
            .tasks
            .complete_playback_results_playable(
                &creation.task.id,
                child_metadata.title,
                "1/2 Bilibili playback result(s) are playable.".to_owned(),
                child_source,
                child_metadata.playback_session,
                result_items,
            )
            .expect("task should become partially playable");

        let direct_parent_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(state.clone())),
            AxumPath(creation.task.id.clone()),
        )
        .await;

        assert_eq!(StatusCode::NOT_FOUND, direct_parent_master.status());
        assert!(state.hls_sessions.get(&creation.task.id).is_none());
        let playable = state
            .tasks
            .get_task(&creation.task.id)
            .expect("playback task should remain readable");
        assert_eq!(TaskState::Playable, playable.state());
        assert_eq!(
            child_session_id,
            playable
                .playback_session
                .as_ref()
                .expect("primary child session should remain selected")
                .id
        );
        assert_eq!(
            i32::from(TaskState::Playable),
            playable.result_items[1].state
        );
        assert!(playable.result_items[1].playback_source.is_some());
        assert!(playable.result_items[1].playback_session.is_some());
    }

    #[tokio::test]
    async fn app_state_serves_restored_completed_child_primary_after_cache_scan_recovers() {
        use std::os::unix::fs::PermissionsExt;

        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let initial_options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://old-media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let state = AppState::new_with_playback_planner(
            initial_options.clone(),
            Arc::new(StaticResolveAndScriptedPlaybackPlanner {
                resolve_requests: Arc::new(Mutex::new(Vec::new())),
                playback_requests: Arc::new(Mutex::new(Vec::new())),
                resolution: sample_resolution_with_pages(),
                results: Mutex::new(HashMap::from([
                    (
                        "page:1".to_owned(),
                        Err(BilibiliDownloadError::Failed(
                            "page 1 planning failed".to_owned(),
                        )),
                    ),
                    (
                        "page:2".to_owned(),
                        Ok(sample_playback_plan_with_video_url(&upstream_url)),
                    ),
                ])),
            }),
        );
        let tasks = Arc::clone(&state.tasks);
        let task_service = TaskGrpcService::new(state);
        let created = task_service
            .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
                url_or_id: "BV1recover-child-completed".to_owned(),
                options: None,
                selection_id: String::new(),
                selection: Some(BilibiliTaskSelection {
                    mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                    selection_ids: Vec::new(),
                    range_start_index: 1,
                    range_end_index: 2,
                }),
            }))
            .await
            .expect("range task should be created")
            .into_inner();

        let completed = wait_for_task_state(&tasks, &created.id, TaskState::Completed).await;
        let child_session_id = format!("{}-result-2", created.id);
        let expected_item_id = format!("bilibili.hls.{child_session_id}");
        let stale_uri =
            format!("http://old-media.example.test:8080/hls/{child_session_id}/master.m3u8");
        assert_eq!(expected_item_id, completed.library_item_id);

        let hls_root = root_path.join(".tvos-net-player").join("hls");
        let mut unreadable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should exist")
            .permissions();
        unreadable_permissions.set_mode(0o000);
        fs::set_permissions(&hls_root, unreadable_permissions)
            .expect("HLS cache root should become unreadable");

        let restored_options = CacheServerOptions {
            public_media_base_uri: Some("http://restored-media.example.test:9090".to_owned()),
            ..initial_options
        };
        let restored =
            AppState::new_with_playback_planner(restored_options, Arc::new(EmptyPlaybackPlanner));
        let preserved = restored
            .tasks
            .get_task(&created.id)
            .expect("completed task should stay persisted while cache scan fails");
        assert_eq!(TaskState::Completed, preserved.state());
        assert_eq!(
            stale_uri,
            preserved
                .playback_source
                .as_ref()
                .expect("playback source should stay stale before lazy recovery")
                .uri
        );
        assert!(restored.hls_sessions.get(&child_session_id).is_none());

        let mut readable_permissions = fs::metadata(&hls_root)
            .expect("HLS cache root should remain present")
            .permissions();
        readable_permissions.set_mode(0o700);
        fs::set_permissions(&hls_root, readable_permissions)
            .expect("HLS cache root should become readable again");

        let direct_master = crate::media::hls_master_playlist_get(
            State(crate::media::MediaState::new(restored.clone())),
            AxumPath(child_session_id.clone()),
        )
        .await;

        assert_eq!(StatusCode::OK, direct_master.status());
        assert!(restored.hls_sessions.get(&child_session_id).is_some());
        let expected_uri =
            format!("http://restored-media.example.test:9090/hls/{child_session_id}/master.m3u8");
        let refreshed = restored
            .tasks
            .get_task(&created.id)
            .expect("completed task should still be available");
        assert_eq!(
            expected_uri,
            refreshed
                .playback_source
                .as_ref()
                .expect("primary source should refresh after lazy recovery")
                .uri
        );
        assert_eq!(expected_item_id, refreshed.library_item_id);
        assert_eq!(
            expected_uri,
            refreshed
                .result_items
                .iter()
                .find(|item| item.id == child_session_id)
                .and_then(|item| item.playback_source.as_ref())
                .expect("child result source should refresh after lazy recovery")
                .uri
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
            .create_bilibili_playback_task("BV1offline", None, None)
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
            .create_bilibili_playback_task("BV1offline", None, None)
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

    #[cfg(unix)]
    #[tokio::test]
    async fn hls_cache_finalizer_transcodes_ready_session_to_generated_runtime() {
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
                lan_transcoding_enabled: true,
                lan_transcoding_ffmpeg_path: write_copying_fake_ffmpeg(temp.path()),
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let (task_id, mut hls_session, library_item_id) =
            create_playable_hls_playback_task(&state, "BV1transcode", &upstream_url);
        mark_hls_session_transcoding_ready(&mut hls_session);
        state
            .hls_cache
            .save_session(&hls_session)
            .expect("transcoding-ready session should persist");
        state.hls_sessions.insert(hls_session.clone());

        run_hls_cache_finalization(
            state.clone(),
            task_id.clone(),
            hls_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        )
        .await;

        let completed = state
            .tasks
            .get_task(&task_id)
            .expect("task should remain readable");
        let runtime_session = state
            .hls_sessions
            .get(&task_id)
            .expect("completed cache should keep a runtime HLS session");
        let restored_session = state
            .hls_cache
            .completed_session(&task_id)
            .expect("completed transcoded session should be persisted");
        let item = state
            .hls_cache
            .get_completed_library_item(&library_item_id)
            .expect("completed transcoded session should expose a library item");

        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(library_item_id, completed.library_item_id);
        let completed_playback_session = completed
            .playback_session
            .as_ref()
            .expect("completed task should expose generated playback session metadata");
        let completed_selected_variant = completed_playback_session
            .selected_variant
            .as_ref()
            .expect("completed task should expose selected generated variant metadata");
        assert_eq!("avc1.64002A", completed_selected_variant.video_codec);
        assert_eq!("mp4a.40.2", completed_selected_variant.audio_codec);
        assert_eq!(
            i32::from(LanTranscodingPlanState::NotRequired),
            completed_playback_session
                .transcoding_plan
                .as_ref()
                .expect("completed task should expose generated transcoding plan")
                .state
        );
        assert_eq!("transcoded.m4s", runtime_session.variant.video.id);
        assert_eq!("transcoded.m4s", restored_session.variant.video.id);
        assert!(
            runtime_session
                .media_playlist_resource("video.m3u8")
                .is_some()
        );
        assert!(
            restored_session
                .media_playlist_resource("video.m3u8")
                .is_some()
        );
        assert!(
            !runtime_session
                .master_playlist()
                .contains("segments/video.m3u8")
        );
        assert!(
            runtime_session
                .master_playlist()
                .contains("segments/transcoded.m3u8")
        );
        assert!(runtime_session.variant.audio.is_none());
        assert!(runtime_session.variant.video.request.url.is_empty());
        let runtime_source_video = runtime_session
            .media_resource("video.m4s")
            .expect("runtime source video lookup should remain addressable");
        assert_eq!(upstream_url, runtime_source_video.request.url);
        assert!(runtime_source_video.request.backup_urls.is_empty());
        assert!(!runtime_source_video.request.headers.is_empty());
        let restored_source_video = restored_session
            .media_resource("video.m4s")
            .expect("persisted source video lookup should remain addressable");
        assert!(restored_source_video.request.url.is_empty());
        assert!(restored_source_video.request.backup_urls.is_empty());
        assert!(restored_source_video.request.headers.is_empty());
        assert_eq!("avc1.64002A", item.variants[0].video_codec);
        assert_eq!("mp4a.40.2", item.variants[0].audio_codec);
        assert_eq!(0, state.lan_transcoding_active_job_count());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn app_state_restore_shortcut_updates_transcoded_playback_metadata() {
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
            lan_transcoding_enabled: true,
            lan_transcoding_ffmpeg_path: write_copying_fake_ffmpeg(temp.path()),
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let creation = state
            .tasks
            .create_bilibili_playback_task("BV1transcode-restore", None, None)
            .expect("playback task should be created");
        let task_id = creation.task.id;
        let metadata = playback_task_metadata_with_options(
            &task_id,
            sample_hevc_playback_plan_with_video_url(&upstream_url),
            &options,
        )
        .expect("HEVC playback metadata should map");
        let hls_session = metadata.hls_session.clone();
        let library_item_id = HlsCacheStore::completed_library_item_id(&task_id);
        state
            .hls_cache
            .save_session(&hls_session)
            .expect("transcoding-ready session should persist");
        state.hls_sessions.insert(hls_session.clone());
        let playback_source = PlaybackSource {
            item_id: task_id.clone(),
            variant_id: metadata.playback_session.selected_variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{task_id}/master.m3u8"),
            expires_at: None,
        };
        state
            .tasks
            .complete_playback_playable(
                &task_id,
                metadata.title,
                playback_source,
                metadata.playback_session,
            )
            .expect("task should become playable");

        let completion = state
            .hls_cache
            .cache_session_resources_completion_with_control(
                &state.hls_upstream_client,
                &hls_session,
                || HlsCacheFillControl::Continue,
                |_| {},
                state.hls_transcoding_execution_config(),
            )
            .await
            .expect("startup crash-window cache completion should persist");
        assert_eq!(library_item_id, completion.library_item_id);
        assert_eq!(
            HlsTranscodingPlanState::NotRequired,
            completion.session.transcoding.state
        );
        assert_eq!("transcoded.m4s", completion.session.variant.video.id);
        let still_playable = state
            .tasks
            .get_task(&task_id)
            .expect("task should still model pre-crash playable state");
        assert_eq!(TaskState::Playable, still_playable.state());
        assert!(
            still_playable
                .playback_session
                .as_ref()
                .and_then(|session| session.transcoding_plan.as_ref())
                .is_some_and(|plan| plan.state == i32::from(LanTranscodingPlanState::Ready))
        );

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let completed = restored
            .tasks
            .get_task(&task_id)
            .expect("task should complete during startup restore");
        let restored_session = restored
            .hls_cache
            .completed_session(&task_id)
            .expect("completed transcoded session should remain persisted");

        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(library_item_id, completed.library_item_id);
        let completed_playback_session = completed
            .playback_session
            .as_ref()
            .expect("startup restore should update completed playback session metadata");
        let completed_selected_variant = completed_playback_session
            .selected_variant
            .as_ref()
            .expect("startup restore should expose generated selected variant metadata");
        assert_eq!("avc1.64002A", completed_selected_variant.video_codec);
        assert_eq!("mp4a.40.2", completed_selected_variant.audio_codec);
        assert_eq!(
            i32::from(LanTranscodingPlanState::NotRequired),
            completed_playback_session
                .transcoding_plan
                .as_ref()
                .expect("startup restore should expose completed transcoding plan")
                .state
        );
        assert_eq!("transcoded.m4s", restored_session.variant.video.id);
    }

    #[test]
    fn hls_cache_session_metadata_classifies_uppercase_aac_codec() {
        let mut adapter_variant = playback_variant("h264", "avc1.640028", 1_000_000, 10_000_000);
        adapter_variant.codecs = vec!["MP4A.40.2".to_owned(), "avc1.640028".to_owned()];
        adapter_variant.audio.as_mut().unwrap().codecs = Some("MP4A.40.2".to_owned());

        let mut session =
            HlsPlaybackSession::from_selected_variant("session-1", "Episode", &adapter_variant)
                .expect("HLS session should be created");
        session.variants[0].codecs = vec!["MP4A.40.2".to_owned(), "avc1.640028".to_owned()];
        session.variants[0].media[0].codecs = Some("avc1.640028".to_owned());
        session.variants[0].media[1].codecs = Some("MP4A.40.2".to_owned());

        let playback_session = playback_session_from_hls_cache_session(&session);
        let selected_variant = playback_session
            .selected_variant
            .as_ref()
            .expect("selected variant metadata should be present");
        let listed_variant = playback_session
            .variants
            .iter()
            .find(|variant| variant.id == "h264")
            .expect("listed variant metadata should be present");

        assert_eq!("avc1.640028", selected_variant.video_codec);
        assert_eq!("MP4A.40.2", selected_variant.audio_codec);
        assert_eq!("avc1.640028", listed_variant.video_codec);
        assert_eq!("MP4A.40.2", listed_variant.audio_codec);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hls_cache_finalizer_does_not_transcode_ready_session_when_disabled() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let ffmpeg_path = write_copying_fake_ffmpeg(temp.path());
        let state = AppState::new_with_playback_planner(
            CacheServerOptions {
                root_path: root_path.clone(),
                task_state_path: root_path.join(".state").join("tasks.json"),
                public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
                bilibili_worker_enabled: false,
                lan_transcoding_enabled: false,
                lan_transcoding_ffmpeg_path: ffmpeg_path,
                ..CacheServerOptions::default()
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let (task_id, mut hls_session, library_item_id) =
            create_playable_hls_playback_task(&state, "BV1transcode-disabled", &upstream_url);
        mark_hls_session_transcoding_ready(&mut hls_session);
        state
            .hls_cache
            .save_session(&hls_session)
            .expect("transcoding-ready session should persist");
        state.hls_sessions.insert(hls_session.clone());

        run_hls_cache_finalization(
            state.clone(),
            task_id.clone(),
            hls_session,
            HlsCacheFinalizationFailureMode::KeepPlayable,
        )
        .await;

        let task = state
            .tasks
            .get_task(&task_id)
            .expect("task should remain readable");

        assert_eq!(TaskState::Playable, task.state());
        assert!(
            state
                .hls_cache
                .get_completed_library_item(&library_item_id)
                .is_none()
        );
        assert!(!temp.path().join("ffmpeg-args.log").exists());
        assert_eq!(0, state.lan_transcoding_active_job_count());
    }

    #[tokio::test]
    async fn app_state_preserves_restored_ready_hls_session_when_lan_transcoding_disabled() {
        let (upstream_url, _upstream_task) = start_mp4_upstream().await;
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let base_options = CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            public_media_base_uri: Some("http://media.example.test:8080".to_owned()),
            bilibili_worker_enabled: false,
            lan_transcoding_enabled: true,
            ..CacheServerOptions::default()
        };
        let state = AppState::new_with_playback_planner(
            base_options.clone(),
            Arc::new(EmptyPlaybackPlanner),
        );
        let (task_id, mut hls_session, library_item_id) = create_playable_hls_playback_task(
            &state,
            "BV1transcode-restore-disabled",
            &upstream_url,
        );
        mark_hls_session_transcoding_ready(&mut hls_session);
        state
            .hls_cache
            .save_session(&hls_session)
            .expect("transcoding-ready session should persist");
        state.hls_sessions.insert(hls_session.clone());

        let restored = AppState::new_with_playback_planner(
            CacheServerOptions {
                lan_transcoding_enabled: false,
                ..base_options
            },
            Arc::new(EmptyPlaybackPlanner),
        );
        let restored_task = restored
            .tasks
            .get_task(&task_id)
            .expect("playable task should survive disabled restore");

        assert_eq!(TaskState::Playable, restored_task.state());
        assert!(restored.hls_sessions.get(&task_id).is_some());
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&library_item_id)
                .is_none()
        );
    }

    #[tokio::test]
    async fn app_state_migrates_completed_ready_source_cache_after_upgrade() {
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
            lan_transcoding_enabled: true,
            ..CacheServerOptions::default()
        };
        let state =
            AppState::new_with_playback_planner(options.clone(), Arc::new(EmptyPlaybackPlanner));
        let completed =
            create_completed_hls_playback_task(&state, "BV1legacy-ready-cache", &upstream_url)
                .await;
        let mut legacy_session = state
            .hls_cache
            .playback_session(&completed.task_id)
            .expect("legacy completed session should remain on disk");
        legacy_session.transcoding = HlsTranscodingPlan::with_state(
            HlsTranscodingPlanState::Ready,
            legacy_session.variant.id.clone(),
            "Legacy completed cache was planned for LAN transcoding before execution was available.",
        );
        state
            .hls_cache
            .save_session(&legacy_session)
            .expect("legacy ready completed session should persist");

        let restored = AppState::new_with_playback_planner(options, Arc::new(EmptyPlaybackPlanner));
        let restored_task = restored
            .tasks
            .get_task(&completed.task_id)
            .expect("completed task should survive legacy ready restore");
        let restored_session = restored
            .hls_cache
            .completed_session(&completed.task_id)
            .expect("legacy ready source cache should be restored as completed");

        assert_eq!(TaskState::Completed, restored_task.state());
        assert_eq!(completed.library_item_id, restored_task.library_item_id);
        assert_eq!(
            HlsTranscodingPlanState::Disabled,
            restored_session.transcoding.state
        );
        assert!(
            restored_task
                .playback_session
                .as_ref()
                .and_then(|session| session.transcoding_plan.as_ref())
                .is_some_and(|plan| plan.state == i32::from(LanTranscodingPlanState::Disabled))
        );
        assert!(
            restored
                .hls_cache
                .get_completed_library_item(&completed.library_item_id)
                .is_some()
        );
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
            .create_bilibili_playback_task("BV1cancelled", None, None)
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
            .create_bilibili_playback_task("BV1corrupt-state", None, None)
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
                selection_id: String::new(),
                selection: None,
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
            .create_bilibili_playback_task("BV1offline", None, None)
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
            .create_bilibili_playback_task("BV1offline", None, None)
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
        assert!(
            !root_path
                .join(".tvos-net-player")
                .join("hls")
                .join(&creation.task.id)
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
                selection_id: String::new(),
                selection: None,
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
                selection_id: String::new(),
                selection: None,
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
                selection_id: String::new(),
                selection: None,
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
                selection_id: String::new(),
                selection: None,
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

    struct CompletedHlsTestTask {
        task_id: String,
        library_item_id: String,
    }

    struct PartialHlsTestTask {
        task_id: String,
    }

    async fn create_completed_hls_playback_task(
        state: &AppState,
        source: &str,
        upstream_url: &str,
    ) -> CompletedHlsTestTask {
        let (task_id, hls_session, _) =
            create_playable_hls_playback_task(state, source, upstream_url);
        let library_item_id = state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &hls_session)
            .await
            .expect("HLS resources should cache");
        state
            .tasks
            .complete_playback_cached(&task_id, library_item_id.clone())
            .expect("task should become completed");
        state
            .hls_sessions
            .insert(sanitized_completed_session(&hls_session));

        CompletedHlsTestTask {
            task_id,
            library_item_id,
        }
    }

    async fn create_partial_hls_playback_task(
        state: &AppState,
        source: &str,
        upstream_url: &str,
    ) -> PartialHlsTestTask {
        let (task_id, mut hls_session, _) =
            create_playable_hls_playback_task(state, source, upstream_url);
        let mut audio = hls_session.variant.video.clone();
        audio.id = "audio.m4s".to_owned();
        audio.request.kind = BilibiliMediaRequestKind::Audio;
        audio.request.codecs = Some("mp4a.40.2".to_owned());
        audio.request.cache_key.media_kind = BilibiliMediaRequestKind::Audio;
        audio.request.cache_key.codecs = Some("mp4a.40.2".to_owned());
        hls_session.variant.audio = Some(audio);
        state.hls_sessions.insert(hls_session.clone());
        let cache_for_preempt = state.hls_cache.clone();
        let task_id_for_preempt = task_id.clone();

        let error = state
            .hls_cache
            .cache_session_resources_with_control(
                &state.hls_upstream_client,
                &hls_session,
                move || {
                    if cache_for_preempt
                        .cached_resource(&task_id_for_preempt, "video.m4s")
                        .is_some()
                    {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                },
                |_| {},
            )
            .await
            .expect_err("partial HLS cache should stop after video resource");
        assert!(matches!(error, crate::hls_cache::HlsCacheError::Preempted));

        PartialHlsTestTask { task_id }
    }

    fn mark_hls_session_transcoding_ready(session: &mut HlsPlaybackSession) {
        let mut audio = session.variant.video.clone();
        audio.id = "audio.m4s".to_owned();
        audio.request.kind = BilibiliMediaRequestKind::Audio;
        audio.request.codecs = Some("mp4a.40.2".to_owned());
        audio.request.cache_key.media_kind = BilibiliMediaRequestKind::Audio;
        audio.request.cache_key.codecs = Some("mp4a.40.2".to_owned());
        audio.request.cache_key.source_hash = "audio-source-hash".to_owned();
        session.variant.audio = Some(audio);
        session.variant.codecs = vec!["hev1.1.6.L120.90".to_owned()];
        session.variant.video.request.codecs = Some("hev1.1.6.L120.90".to_owned());
        session.variant.video.request.cache_key.codecs = Some("hev1.1.6.L120.90".to_owned());
        session.variant.video.request.cache_key.source_hash = "hevc-source-hash".to_owned();
        session.transcoding = HlsTranscodingPlan::with_state(
            HlsTranscodingPlanState::Ready,
            session.variant.id.clone(),
            "HEVC source should be converted before completed offline cache exposure.",
        );
    }

    fn create_playable_hls_playback_task(
        state: &AppState,
        source: &str,
        upstream_url: &str,
    ) -> (String, HlsPlaybackSession, String) {
        let creation = state
            .tasks
            .create_bilibili_playback_task(source, None, None)
            .expect("playback task should be created");
        let metadata = playback_task_metadata(
            &creation.task.id,
            sample_playback_plan_with_video_url(upstream_url),
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
        let task_id = creation.task.id;
        let library_item_id = HlsCacheStore::completed_library_item_id(&task_id);
        (task_id, metadata.hls_session, library_item_id)
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

    struct FailingPlaybackPlanner {
        detail: String,
    }

    impl BilibiliPlaybackPlanner for FailingPlaybackPlanner {
        fn resolve_input<'a>(
            &'a self,
            _request: BilibiliInputResolveRequest,
        ) -> BilibiliInputResolveFuture<'a> {
            let detail = self.detail.clone();
            Box::pin(async move { Err(BilibiliDownloadError::Failed(detail)) })
        }

        fn plan<'a>(
            &'a self,
            _request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            let detail = self.detail.clone();
            Box::pin(async move { Err(BilibiliDownloadError::Failed(detail)) })
        }
    }

    type ResolveRequestLog = Arc<Mutex<Vec<(String, Option<BilibiliPlaybackOptions>)>>>;
    type PlaybackRequestLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

    struct StaticResolvePlanner {
        requests: ResolveRequestLog,
        resolution: BilibiliInputResolution,
    }

    impl BilibiliPlaybackPlanner for StaticResolvePlanner {
        fn resolve_input<'a>(
            &'a self,
            request: BilibiliInputResolveRequest,
        ) -> BilibiliInputResolveFuture<'a> {
            self.requests
                .lock()
                .expect("request log should not be poisoned")
                .push((request.source, request.options));
            let resolution = self.resolution.clone();
            Box::pin(async move { Ok(resolution) })
        }

        fn plan<'a>(
            &'a self,
            _request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            Box::pin(async { Ok(sample_playback_plan()) })
        }
    }

    struct StaticResolveAndRecordingPlaybackPlanner {
        resolve_requests: ResolveRequestLog,
        playback_requests: PlaybackRequestLog,
        resolution: BilibiliInputResolution,
    }

    impl BilibiliPlaybackPlanner for StaticResolveAndRecordingPlaybackPlanner {
        fn resolve_input<'a>(
            &'a self,
            request: BilibiliInputResolveRequest,
        ) -> BilibiliInputResolveFuture<'a> {
            self.resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
                .push((request.source, request.options));
            let resolution = self.resolution.clone();
            Box::pin(async move { Ok(resolution) })
        }

        fn plan<'a>(
            &'a self,
            request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            self.playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
                .push((request.source, request.selection_id));
            Box::pin(async { Ok(sample_playback_plan()) })
        }
    }

    struct StaticResolveAndScriptedPlaybackPlanner {
        resolve_requests: ResolveRequestLog,
        playback_requests: PlaybackRequestLog,
        resolution: BilibiliInputResolution,
        results: Mutex<HashMap<String, PlaybackPlanningTestResult>>,
    }

    impl BilibiliPlaybackPlanner for StaticResolveAndScriptedPlaybackPlanner {
        fn resolve_input<'a>(
            &'a self,
            request: BilibiliInputResolveRequest,
        ) -> BilibiliInputResolveFuture<'a> {
            self.resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
                .push((request.source, request.options));
            let resolution = self.resolution.clone();
            Box::pin(async move { Ok(resolution) })
        }

        fn plan<'a>(
            &'a self,
            request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            let selection_id = request
                .selection_id
                .clone()
                .expect("scripted planner should receive an explicit selection id");
            self.playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
                .push((request.source, Some(selection_id.clone())));
            let result = self
                .results
                .lock()
                .expect("scripted playback results should not be poisoned")
                .remove(&selection_id)
                .expect("scripted playback result should exist");
            Box::pin(async move { result })
        }
    }

    struct CancelDuringSecondSelectionPlaybackPlanner {
        resolve_requests: ResolveRequestLog,
        playback_requests: PlaybackRequestLog,
        resolution: BilibiliInputResolution,
        first_planned: Mutex<Option<oneshot::Sender<()>>>,
        second_started: Mutex<Option<oneshot::Sender<()>>>,
    }

    impl BilibiliPlaybackPlanner for CancelDuringSecondSelectionPlaybackPlanner {
        fn resolve_input<'a>(
            &'a self,
            request: BilibiliInputResolveRequest,
        ) -> BilibiliInputResolveFuture<'a> {
            self.resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
                .push((request.source, request.options));
            let resolution = self.resolution.clone();
            Box::pin(async move { Ok(resolution) })
        }

        fn plan<'a>(
            &'a self,
            request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            let selection_id = request
                .selection_id
                .clone()
                .expect("cancellable planner should receive an explicit selection id");
            self.playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
                .push((request.source.clone(), Some(selection_id.clone())));
            match selection_id.as_str() {
                "page:1" => {
                    let first_planned = self
                        .first_planned
                        .lock()
                        .expect("first-planned signal lock should not be poisoned")
                        .take();
                    Box::pin(async move {
                        if let Some(sender) = first_planned {
                            let _ = sender.send(());
                        }
                        Ok(sample_playback_plan())
                    })
                }
                "page:2" => {
                    let second_started = self
                        .second_started
                        .lock()
                        .expect("second-started signal lock should not be poisoned")
                        .take();
                    Box::pin(async move {
                        if let Some(sender) = second_started {
                            let _ = sender.send(());
                        }
                        while !request.cancellation.is_cancel_requested() {
                            sleep(Duration::from_millis(10)).await;
                        }
                        Err(BilibiliDownloadError::Cancelled(
                            "Playback planning was cancelled.".to_owned(),
                        ))
                    })
                }
                other => {
                    let unexpected = other.to_owned();
                    Box::pin(async move {
                        Err(BilibiliDownloadError::Failed(format!(
                            "Unexpected selection id {unexpected}"
                        )))
                    })
                }
            }
        }
    }

    struct CancelDuringResolutionPlanner {
        resolve_requests: ResolveRequestLog,
        playback_requests: PlaybackRequestLog,
        resolve_started: Mutex<Option<oneshot::Sender<()>>>,
    }

    impl BilibiliPlaybackPlanner for CancelDuringResolutionPlanner {
        fn resolve_input<'a>(
            &'a self,
            request: BilibiliInputResolveRequest,
        ) -> BilibiliInputResolveFuture<'a> {
            self.resolve_requests
                .lock()
                .expect("resolve request log should not be poisoned")
                .push((request.source, request.options));
            let resolve_started = self
                .resolve_started
                .lock()
                .expect("resolve-started signal lock should not be poisoned")
                .take();
            Box::pin(async move {
                if let Some(sender) = resolve_started {
                    let _ = sender.send(());
                }
                while !request.cancellation.is_cancel_requested() {
                    sleep(Duration::from_millis(10)).await;
                }
                Err(BilibiliDownloadError::Cancelled(
                    "Input resolution was cancelled.".to_owned(),
                ))
            })
        }

        fn plan<'a>(
            &'a self,
            request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            self.playback_requests
                .lock()
                .expect("playback request log should not be poisoned")
                .push((request.source, request.selection_id));
            Box::pin(async { Ok(sample_playback_plan()) })
        }
    }

    struct RecordingPlaybackPlanner {
        requests: PlaybackRequestLog,
    }

    impl BilibiliPlaybackPlanner for RecordingPlaybackPlanner {
        fn plan<'a>(
            &'a self,
            request: BilibiliPlaybackPlanningRequest,
        ) -> BilibiliPlaybackPlanningFuture<'a> {
            self.requests
                .lock()
                .expect("request log should not be poisoned")
                .push((request.source, request.selection_id));
            Box::pin(async { Ok(sample_playback_plan()) })
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
        wait_for_task_condition(tasks, task_id, |task| task.state() == expected_state).await
    }

    async fn wait_for_task_condition(
        tasks: &BilibiliTaskRegistry,
        task_id: &str,
        predicate: impl Fn(&Task) -> bool,
    ) -> Task {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let task = tasks
                    .get_task(task_id)
                    .expect("task should exist while waiting for state");
                if predicate(&task) {
                    return task;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task should reach expected state")
    }

    fn sample_playback_plan() -> BilibiliPlaybackPlan {
        let mut selected_variant = playback_variant("h264", "avc1.640028", 1_000_000, 10_000_000);
        selected_variant.abr = Some(playback_abr_level(0, 2));
        let mut alternate_variant =
            playback_variant("hevc", "hvc1.1.6.L120.90", 2_000_000, 20_000_000);
        alternate_variant.abr = Some(playback_abr_level(1, 2));
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
                abr: sample_playback_abr_metadata(vec!["h264", "hevc"], 1_000_000, 2_000_000),
                selected_variant: Some(BilibiliSelectedPlaybackVariant {
                    variant: selected_variant.clone(),
                    selection: BilibiliPlaybackVariantSelection {
                        policy: BilibiliPlaybackVariantSelectionPolicy::AvPlayerDefault,
                        codec_rank: Some(1),
                        score: 100,
                    },
                }),
                variants: vec![selected_variant, alternate_variant],
            }],
        }
    }

    fn sample_resolution_with_pages() -> BilibiliInputResolution {
        BilibiliInputResolution {
            source: "BV1range".to_owned(),
            title: "Range video".to_owned(),
            source_kind: "video".to_owned(),
            default_selection_id: String::new(),
            candidates_truncated: false,
            candidates: vec![
                BilibiliResolvedCandidate {
                    selection_id: "page:1".to_owned(),
                    title: "Part 1".to_owned(),
                    subtitle: "Page 1".to_owned(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-1".to_owned(),
                    index: 1,
                    duration_seconds: Some(60),
                    cover_uri: String::new(),
                },
                BilibiliResolvedCandidate {
                    selection_id: "page:2".to_owned(),
                    title: "Part 2".to_owned(),
                    subtitle: "Page 2".to_owned(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-2".to_owned(),
                    index: 2,
                    duration_seconds: Some(61),
                    cover_uri: String::new(),
                },
                BilibiliResolvedCandidate {
                    selection_id: "page:3".to_owned(),
                    title: "Part 3".to_owned(),
                    subtitle: "Page 3".to_owned(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-3".to_owned(),
                    index: 3,
                    duration_seconds: Some(62),
                    cover_uri: String::new(),
                },
            ],
        }
    }

    fn sample_playback_plan_with_video_url(url: &str) -> BilibiliPlaybackPlan {
        let mut selected_variant = playback_variant_with_url("h264", "avc1.640028", 1_000_000, url);
        selected_variant.abr = Some(playback_abr_level(0, 1));
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
                abr: sample_playback_abr_metadata(vec!["h264"], 1_000_000, 1_000_000),
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

    fn sample_hevc_playback_plan_with_video_url(url: &str) -> BilibiliPlaybackPlan {
        let mut selected_variant =
            playback_variant_with_url("hevc", "hev1.1.6.L120.90", 2_000_000, url);
        selected_variant.abr = Some(playback_abr_level(0, 1));
        selected_variant.audio = Some(media_request_with_url(
            BilibiliMediaRequestKind::Audio,
            "mp4a.40.2",
            url,
        ));
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
                abr: sample_playback_abr_metadata(vec!["hevc"], 2_000_000, 2_000_000),
                selected_variant: Some(BilibiliSelectedPlaybackVariant {
                    variant: selected_variant.clone(),
                    selection: BilibiliPlaybackVariantSelection {
                        policy: BilibiliPlaybackVariantSelectionPolicy::ExplicitEncodingPreference,
                        codec_rank: Some(1),
                        score: 100,
                    },
                }),
                variants: vec![selected_variant],
            }],
        }
    }

    fn sample_playback_plan_with_alternate_video_urls(
        selected_url: &str,
        alternate_url: &str,
    ) -> BilibiliPlaybackPlan {
        let mut selected_variant =
            playback_variant_with_url("h264", "avc1.640028", 1_000_000, selected_url);
        selected_variant.abr = Some(playback_abr_level(0, 2));
        let mut alternate_variant =
            playback_variant_with_url("h264-720p", "avc1.640028", 600_000, alternate_url);
        alternate_variant.width = Some(1280);
        alternate_variant.height = Some(720);
        alternate_variant.abr = Some(playback_abr_level(1, 2));
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
                abr: sample_playback_abr_metadata(vec!["h264", "h264-720p"], 600_000, 1_000_000),
                selected_variant: Some(BilibiliSelectedPlaybackVariant {
                    variant: selected_variant.clone(),
                    selection: BilibiliPlaybackVariantSelection {
                        policy: BilibiliPlaybackVariantSelectionPolicy::AvPlayerDefault,
                        codec_rank: Some(1),
                        score: 100,
                    },
                }),
                variants: vec![selected_variant, alternate_variant],
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

    fn sample_playback_abr_metadata(
        variant_ids: Vec<&str>,
        min_bandwidth: u64,
        max_bandwidth: u64,
    ) -> BilibiliPlaybackAbrMetadata {
        let level_count = variant_ids.len().try_into().unwrap_or(u32::MAX);
        BilibiliPlaybackAbrMetadata {
            groups: vec![BilibiliPlaybackAbrGroup {
                id: "dash-video".to_owned(),
                kind: BilibiliPlaybackAbrGroupKind::DashVideo,
                variant_ids: variant_ids
                    .into_iter()
                    .map(std::borrow::ToOwned::to_owned)
                    .collect(),
                level_count,
                min_bandwidth: Some(min_bandwidth),
                max_bandwidth: Some(max_bandwidth),
            }],
        }
    }

    fn playback_abr_level(level_index: u32, level_count: u32) -> BilibiliPlaybackAbrLevel {
        BilibiliPlaybackAbrLevel {
            group_id: "dash-video".to_owned(),
            level_index,
            level_count,
            switchable: true,
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

    #[cfg(unix)]
    fn write_copying_fake_ffmpeg(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("fake-ffmpeg-copy");
        std::fs::write(
            &path,
            r#"#!/bin/sh
set -eu
last=
input=
previous=
for arg in "$@"; do
  if [ "$previous" = "-i" ] && [ -z "$input" ]; then
    input=$arg
  fi
  last=$arg
  previous=$arg
done
cp "$input" "$last"
"#,
        )
        .expect("fake ffmpeg should be written");
        let mut permissions = std::fs::metadata(&path)
            .expect("fake ffmpeg metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("fake ffmpeg should be executable");
        path
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
