use std::{
    io::SeekFrom,
    ops::Range as StdRange,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    BoxError,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode,
        header::{
            ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
            IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED, RANGE, RETRY_AFTER,
        },
    },
};
use futures_util::{StreamExt, TryStreamExt, stream::BoxStream};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    time::Instant,
};
use tokio_util::io::ReaderStream;

use crate::{
    AppState,
    cdn_history::{CdnObservation, CdnObservationOutcome, CdnObservationSource},
    hls::{
        HlsMediaResource, HlsMediaSegment, HlsPlaybackSession, mp4_initialization_length,
        should_forward_media_request_header,
    },
    hls_cache::{HlsCacheFillControl, OpenedPrewarmedHlsResource},
    hls_media_refresh::is_expired_range_error,
    hls_range_cache::{HlsRangeError, HlsRangePriority, HlsReadyRange},
    library::OpenedMediaFile,
    playback_policy::WeakNetworkPreference,
};

const HLS_INITIALIZATION_SCAN_BYTES: u64 = 1024 * 1024;
const HLS_MEDIA_STREAM_CHUNK_BYTES: u64 = 256 * 1024;
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");
static HLS_MEDIA_404_DIAGNOSTIC_BUDGET: AtomicUsize = AtomicUsize::new(32);

#[derive(Clone, Copy)]
enum HlsMediaNotFoundBranch {
    RegisteredSessionRejected,
    SessionMissingOrUnrestorable,
    PlaylistResourceLookupMiss,
    PlaylistResourceUnavailable,
    MediaResourceLookupMiss,
    MediaResourceUnavailable,
    RangeCancelled,
    RangeSessionRemoving,
}

impl HlsMediaNotFoundBranch {
    fn as_str(self) -> &'static str {
        match self {
            Self::RegisteredSessionRejected => "registered_session_rejected_by_serving_gate",
            Self::SessionMissingOrUnrestorable => "session_missing_or_unrestorable",
            Self::PlaylistResourceLookupMiss => "playlist_resource_lookup_miss",
            Self::PlaylistResourceUnavailable => "playlist_resource_unavailable",
            Self::MediaResourceLookupMiss => "media_resource_lookup_miss",
            Self::MediaResourceUnavailable => "media_resource_unavailable",
            Self::RangeCancelled => "range_cancelled",
            Self::RangeSessionRemoving => "range_session_removing",
        }
    }
}

fn log_hls_media_not_found(branch: HlsMediaNotFoundBranch) {
    if HLS_MEDIA_404_DIAGNOSTIC_BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
    {
        eprintln!("HLS_MEDIA_404 branch={}", branch.as_str());
    }
}

#[derive(Clone)]
pub struct MediaState {
    state: Arc<AppState>,
}

impl MediaState {
    pub fn new(state: AppState) -> Self {
        Self {
            state: Arc::new(state),
        }
    }

    pub(crate) fn bilibili_login_context(
        &self,
    ) -> (
        &crate::bilibili_login::BilibiliLoginManager,
        &crate::config::CacheServerOptions,
    ) {
        (&self.state.bilibili_login, &self.state.options)
    }
}

pub async fn media_get(
    State(state): State<MediaState>,
    Path((item_id, variant_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    media_response(state, item_id, variant_id, headers, false).await
}

pub async fn media_head(
    State(state): State<MediaState>,
    Path((item_id, variant_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    media_response(state, item_id, variant_id, headers, true).await
}

pub async fn resource_get(
    State(state): State<MediaState>,
    Path(resource_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    resource_response(state, resource_id, headers, false).await
}

pub async fn resource_head(
    State(state): State<MediaState>,
    Path(resource_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    resource_response(state, resource_id, headers, true).await
}

pub async fn hls_master_playlist_get(
    State(state): State<MediaState>,
    Path(session_id): Path<String>,
) -> Response<Body> {
    hls_master_playlist_response(state, session_id, false)
}

pub async fn hls_master_playlist_head(
    State(state): State<MediaState>,
    Path(session_id): Path<String>,
) -> Response<Body> {
    hls_master_playlist_response(state, session_id, true)
}

pub async fn hls_segment_get(
    State(state): State<MediaState>,
    Path((session_id, segment_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    hls_segment_response(state, session_id, segment_id, headers, false).await
}

pub async fn hls_segment_head(
    State(state): State<MediaState>,
    Path((session_id, segment_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    hls_segment_response(state, session_id, segment_id, headers, true).await
}

async fn media_response(
    state: MediaState,
    item_id: String,
    variant_id: String,
    headers: HeaderMap,
    head_only: bool,
) -> Response<Body> {
    let Some(opened_file) = state
        .state
        .library
        .open_media_file(&item_id, &variant_id)
        .await
    else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    let range = match parse_range(headers.get(RANGE), opened_file.size_bytes) {
        Ok(range) => range,
        Err(_) => {
            let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
            response.headers_mut().insert(
                CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{}", opened_file.size_bytes))
                    .expect("content range header should be valid"),
            );
            return response;
        }
    };

    build_file_response(opened_file, range, head_only).await
}

async fn resource_response(
    state: MediaState,
    resource_id: String,
    headers: HeaderMap,
    head_only: bool,
) -> Response<Body> {
    let permit = match Arc::clone(&state.state.task_resource_open_permits).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return resource_open_busy_response(),
    };
    let tasks = Arc::clone(&state.state.tasks);
    let opened_resource = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        tasks.open_task_resource(&resource_id)
    })
    .await;
    let opened_resource = match opened_resource {
        Ok(Ok(Some(opened_resource))) => opened_resource,
        Ok(Ok(None)) => return resource_not_found_response(),
        Ok(Err(error)) => {
            eprintln!("Task resource storage open failed: {error}");
            return resource_open_busy_response();
        }
        Err(error) => {
            eprintln!("Task resource open worker failed: {error}");
            return resource_open_busy_response();
        }
    };
    let resource = opened_resource.record.resource;

    if resource.size_known
        && u64::try_from(resource.size_bytes).ok() != Some(opened_resource.size_bytes)
    {
        return resource_not_found_response();
    }

    let etag = quoted_etag_header_value(&resource.etag);
    if resource_is_not_modified(&headers, etag.as_ref(), opened_resource.last_modified) {
        return resource_not_modified_response(
            &resource.content_type,
            resource.supports_byte_ranges,
            etag,
            opened_resource.last_modified,
        );
    }

    let range_header = if !head_only
        && resource.supports_byte_ranges
        && range_validator_matches(&headers, etag.as_ref(), opened_resource.last_modified)
    {
        match single_range_header(&headers) {
            Ok(range_header) => range_header,
            Err(_) => {
                return resource_range_not_satisfiable_response(
                    opened_resource.size_bytes,
                    &resource.content_type,
                    resource.supports_byte_ranges,
                    &resource.etag,
                );
            }
        }
    } else {
        None
    };
    let requested_range = match parse_range(range_header, opened_resource.size_bytes) {
        Ok(range) => range,
        Err(_) => {
            return resource_range_not_satisfiable_response(
                opened_resource.size_bytes,
                &resource.content_type,
                resource.supports_byte_ranges,
                &resource.etag,
            );
        }
    };
    let opened_file = OpenedMediaFile {
        file: opened_resource.file,
        content_type: resource.content_type,
        last_modified: opened_resource.last_modified,
        size_bytes: opened_resource.size_bytes,
    };
    let mut response = build_file_response(opened_file, requested_range, head_only).await;
    if !response.status().is_success() {
        return resource_not_found_response();
    }
    apply_resource_headers(
        response.headers_mut(),
        resource.supports_byte_ranges,
        &resource.etag,
    );
    response
}

fn hls_master_playlist_response(
    state: MediaState,
    session_id: String,
    head_only: bool,
) -> Response<Body> {
    let session_was_registered = state.state.hls_sessions.get(&session_id).is_some();
    let Some(handle) = state.state.hls_playback_session_for_serving(&session_id) else {
        log_hls_media_not_found(if session_was_registered {
            HlsMediaNotFoundBranch::RegisteredSessionRejected
        } else {
            HlsMediaNotFoundBranch::SessionMissingOrUnrestorable
        });
        return empty_response(StatusCode::NOT_FOUND);
    };
    let generation = handle.generation;
    let session = handle.session;

    let mut variants = vec![session.variant.clone()];
    if session.advertise_alternate_variants {
        variants.extend(session.alternate_variants.iter().cloned());
    }
    variants.retain(|variant| {
        [&variant.video]
            .into_iter()
            .chain(variant.audio.iter())
            .all(|resource| {
                resource_has_upstream(resource)
                    || state
                        .state
                        .hls_cache
                        .open_cached_resource(&session_id, &resource.id)
                        .is_some()
            })
    });
    if variants.is_empty() {
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "No complete HLS representation is available offline.\n",
            head_only,
        );
    }
    let selected_index = variants
        .iter()
        .position(|variant| variant.id == session.variant.id)
        .unwrap_or(0);
    let mut playlist_session = session.clone();
    playlist_session.variant = variants.remove(selected_index);
    playlist_session.alternate_variants = variants;
    playlist_session.advertise_alternate_variants = true;
    let weak_network_preference = session.effective_policy.weak_network_preference;
    let body = if head_only {
        Body::empty()
    } else {
        Body::from(
            playlist_session.master_playlist_with_variant_filter(|variant| {
                state
                    .state
                    .hls_network_policy
                    .variant_is_advertisable_for_policy(
                        weak_network_preference,
                        &session_id,
                        generation,
                        &variant.id,
                    )
            }),
        )
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(CACHE_CONTROL, "no-store")
        .body(body)
        .expect("HLS master playlist response should build")
}

async fn hls_segment_response(
    state: MediaState,
    session_id: String,
    segment_id: String,
    headers: HeaderMap,
    head_only: bool,
) -> Response<Body> {
    let session_was_registered = state.state.hls_sessions.get(&session_id).is_some();
    let Some(handle) = state.state.hls_playback_session_for_serving(&session_id) else {
        log_hls_media_not_found(if session_was_registered {
            HlsMediaNotFoundBranch::RegisteredSessionRejected
        } else {
            HlsMediaNotFoundBranch::SessionMissingOrUnrestorable
        });
        return empty_response(StatusCode::NOT_FOUND);
    };
    let generation = handle.generation;
    let session = handle.session;
    let weak_network_preference = session.effective_policy.weak_network_preference;
    let policy_recorder = HlsNetworkPolicyRecorder::new(
        &state,
        session_id.clone(),
        generation,
        weak_network_preference,
    );

    if segment_id.ends_with(".m3u8") {
        let (variant_id, resource, advertised_resource) = if let Some((variant_id, resource)) =
            session.servable_media_playlist_resource_with_variant(&segment_id)
        {
            (variant_id, resource, true)
        } else if let Some((variant_id, resource)) =
            session.lookup_media_playlist_resource_with_variant(&segment_id)
        {
            (variant_id, resource, false)
        } else {
            log_hls_media_not_found(HlsMediaNotFoundBranch::PlaylistResourceLookupMiss);
            return empty_response(StatusCode::NOT_FOUND);
        };
        if !hls_variant_is_servable_for_request(
            &state,
            &session,
            &session_id,
            generation,
            &variant_id,
            advertised_resource,
        ) {
            return weak_network_variant_unavailable_response(head_only);
        }
        let initialization = if let Some(cached) = state
            .state
            .hls_cache
            .cached_resource(&session_id, &resource.id)
        {
            policy_recorder.record_cache_hit();
            Mp4Initialization {
                length: cached.initialization_length,
                total_length: cached.total_length,
                segments: cached.segments,
            }
        } else if !advertised_resource && !resource_has_upstream(&resource) {
            log_hls_media_not_found(HlsMediaNotFoundBranch::PlaylistResourceUnavailable);
            return empty_response(StatusCode::NOT_FOUND);
        } else if let Some(prewarmed) = state
            .state
            .hls_cache
            .prewarmed_resource(&session_id, &resource.id)
        {
            policy_recorder.record_cache_hit();
            Mp4Initialization {
                length: prewarmed.initialization_length,
                total_length: prewarmed.total_length,
                segments: Vec::new(),
            }
        } else {
            let Ok(initialization) =
                load_hls_mp4_initialization(&state, &policy_recorder, &variant_id, &resource).await
            else {
                policy_recorder.record_upstream_failure(&variant_id);
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    "HLS upstream MP4 initialization probe failed.\n",
                    head_only,
                );
            };
            initialization
        };
        let playlist = if advertised_resource {
            session.servable_media_playlist(
                &segment_id,
                initialization.length,
                initialization.total_length,
                &initialization.segments,
            )
        } else {
            session.lookup_media_playlist(
                &segment_id,
                initialization.length,
                initialization.total_length,
                &initialization.segments,
            )
        };
        let Some(playlist) = playlist else {
            return text_response(
                StatusCode::BAD_GATEWAY,
                "HLS upstream MP4 initialization range was invalid.\n",
                head_only,
            );
        };
        let body = if head_only {
            Body::empty()
        } else {
            Body::from(playlist)
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "application/vnd.apple.mpegurl")
            .header(CACHE_CONTROL, "no-store")
            .body(body)
            .expect("HLS media playlist response should build");
    }

    let (variant_id, resource, advertised_resource) = if let Some((variant_id, resource)) =
        session.servable_media_resource_with_variant(&segment_id)
    {
        (variant_id, resource, true)
    } else if let Some((variant_id, resource)) = session.media_resource_with_variant(&segment_id) {
        (variant_id, resource, false)
    } else {
        log_hls_media_not_found(HlsMediaNotFoundBranch::MediaResourceLookupMiss);
        return empty_response(StatusCode::NOT_FOUND);
    };

    if !hls_variant_is_servable_for_request(
        &state,
        &session,
        &session_id,
        generation,
        &variant_id,
        advertised_resource,
    ) {
        return weak_network_variant_unavailable_response(head_only);
    }

    if head_only {
        let range_header = match single_range_header(&headers) {
            Ok(range) => range,
            Err(()) => return hls_range_not_satisfiable_response(resource.request.size),
        };
        if range_header.is_some_and(|header| !range_header_is_well_formed(header)) {
            return hls_range_not_satisfiable_response(resource.request.size);
        }
        if let (Some(header), Some(size)) = (range_header, resource.request.size)
            && parse_range(Some(header), size).is_err()
        {
            return hls_range_not_satisfiable_response(Some(size));
        }
    }

    if let Some(opened_file) = state
        .state
        .hls_cache
        .open_cached_resource(&session_id, &resource.id)
    {
        let range_header = match single_range_header(&headers) {
            Ok(range) => range,
            Err(()) => {
                return resource_range_not_satisfiable_response(
                    opened_file.size_bytes,
                    resource.content_type(),
                    true,
                    "",
                );
            }
        };
        let range = match parse_range(range_header, opened_file.size_bytes) {
            Ok(range) => range,
            Err(_) => {
                let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
                response.headers_mut().insert(
                    CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{}", opened_file.size_bytes))
                        .expect("content range header should be valid"),
                );
                return response;
            }
        };
        policy_recorder.record_cache_hit();
        return build_file_response(opened_file, range, head_only).await;
    }

    if !advertised_resource && !resource_has_upstream(&resource) {
        log_hls_media_not_found(HlsMediaNotFoundBranch::MediaResourceUnavailable);
        return empty_response(StatusCode::NOT_FOUND);
    }

    if let Some(opened_file) = state
        .state
        .hls_cache
        .open_prewarmed_resource(&session_id, &resource.id)
    {
        let range_header = match single_range_header(&headers) {
            Ok(range) => range,
            Err(()) => {
                let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
                response.headers_mut().insert(
                    CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{}", opened_file.total_length))
                        .expect("content-range header should be valid"),
                );
                return response;
            }
        };
        if let Some(range_header) = range_header {
            let range = match parse_range(Some(range_header), opened_file.total_length) {
                Ok(Some(range)) if range.end < opened_file.prefix_length => {
                    policy_recorder.record_cache_hit();
                    range
                }
                Ok(Some(range)) if range.start < opened_file.prefix_length => {
                    return build_prewarmed_spliced_file_response(
                        HlsMediaProxyContext {
                            state: &state,
                            variant_id: &variant_id,
                            resource: &resource,
                            policy_recorder: &policy_recorder,
                        },
                        opened_file,
                        range,
                        &headers,
                        head_only,
                    )
                    .await;
                }
                Ok(_) => {
                    return proxy_hls_media_resource(
                        &state,
                        &policy_recorder,
                        &variant_id,
                        resource,
                        &headers,
                        head_only,
                    )
                    .await;
                }
                Err(_) => {
                    let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
                    response.headers_mut().insert(
                        CONTENT_RANGE,
                        HeaderValue::from_str(&format!("bytes */{}", opened_file.total_length))
                            .expect("content range header should be valid"),
                    );
                    return response;
                }
            };
            return build_prewarmed_file_response(opened_file, range, head_only).await;
        }
    }

    if head_only {
        proxy_hls_media_resource(
            &state,
            &policy_recorder,
            &variant_id,
            resource,
            &headers,
            true,
        )
        .await
    } else {
        foreground_hls_range_response(
            &state,
            &policy_recorder,
            generation,
            &variant_id,
            resource,
            &headers,
        )
        .await
    }
}

fn hls_variant_is_servable_for_request(
    state: &MediaState,
    session: &HlsPlaybackSession,
    session_id: &str,
    generation: u64,
    variant_id: &str,
    advertised_resource: bool,
) -> bool {
    let weak_network_preference = session.effective_policy.weak_network_preference;
    if !advertised_resource || weak_network_preference != WeakNetworkPreference::HoldDowngrade {
        return true;
    }

    session.variant_is_advertised_with_filter(variant_id, |variant| {
        state
            .state
            .hls_network_policy
            .variant_is_advertisable_for_policy(
                weak_network_preference,
                session_id,
                generation,
                &variant.id,
            )
    })
}

fn weak_network_variant_unavailable_response(head_only: bool) -> Response<Body> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(CACHE_CONTROL, "no-store")
        .body(if head_only {
            Body::empty()
        } else {
            Body::from("HLS variant unavailable under the active weak-network policy.\n")
        })
        .expect("HLS weak-network response should build")
}

#[derive(Clone)]
struct HlsNetworkPolicyRecorder {
    state: Arc<AppState>,
    session_id: String,
    generation: u64,
    weak_network_preference: WeakNetworkPreference,
}

impl HlsNetworkPolicyRecorder {
    fn new(
        state: &MediaState,
        session_id: String,
        generation: u64,
        weak_network_preference: WeakNetworkPreference,
    ) -> Self {
        Self {
            state: Arc::clone(&state.state),
            session_id,
            generation,
            weak_network_preference,
        }
    }

    fn record_cache_hit(&self) {
        self.state.hls_sessions.with_network_policy_update(
            &self.session_id,
            self.generation,
            || {
                self.state.hls_network_policy.record_cache_hit_for_policy(
                    self.weak_network_preference,
                    &self.session_id,
                    self.generation,
                );
            },
        );
    }

    fn record_upstream_retry(&self, variant_id: &str) {
        self.state.hls_sessions.with_network_policy_update(
            &self.session_id,
            self.generation,
            || {
                self.state
                    .hls_network_policy
                    .record_upstream_retry_for_policy(
                        self.weak_network_preference,
                        &self.session_id,
                        self.generation,
                        variant_id,
                    );
            },
        );
    }

    fn record_upstream_success(&self, variant_id: &str, response_time: Duration) {
        self.state.hls_sessions.with_network_policy_update(
            &self.session_id,
            self.generation,
            || {
                self.state
                    .hls_network_policy
                    .record_upstream_success_for_policy(
                        self.weak_network_preference,
                        &self.session_id,
                        self.generation,
                        variant_id,
                        response_time,
                    );
            },
        );
    }

    fn record_upstream_failure(&self, variant_id: &str) {
        self.state.hls_sessions.with_network_policy_update(
            &self.session_id,
            self.generation,
            || {
                self.state
                    .hls_network_policy
                    .record_upstream_failure_for_policy(
                        self.weak_network_preference,
                        &self.session_id,
                        self.generation,
                        variant_id,
                    );
            },
        );
    }
}

async fn foreground_hls_range_response(
    state: &MediaState,
    policy_recorder: &HlsNetworkPolicyRecorder,
    generation: u64,
    variant_id: &str,
    resource: HlsMediaResource,
    headers: &HeaderMap,
) -> Response<Body> {
    let range_header = match single_range_header(headers) {
        Ok(range) => range.cloned(),
        Err(()) => return hls_range_not_satisfiable_response(resource.request.size),
    };
    if range_header
        .as_ref()
        .is_some_and(|header| !range_header_is_well_formed(header))
    {
        return hls_range_not_satisfiable_response(resource.request.size);
    }

    let control = hls_range_control(
        Arc::clone(&state.state),
        policy_recorder.session_id.clone(),
        generation,
        resource.clone(),
    );
    let mut total_length = resource.request.size;
    let mut initial_probe = None;
    if total_length.is_none() {
        let was_cached = match state
            .state
            .hls_cache
            .read_resource_range(&policy_recorder.session_id, &resource, 0..1)
            .await
        {
            Ok(bytes) => bytes.is_some(),
            Err(error) => {
                policy_recorder.record_upstream_failure(variant_id);
                return hls_range_error_response(error);
            }
        };
        match ensure_hls_range_with_media_refresh(
            state,
            &policy_recorder.session_id,
            &resource,
            0..1,
            HlsRangePriority::Foreground,
            &control,
        )
        .await
        {
            Ok(ready) => match state
                .state
                .hls_cache
                .read_resource_range(&policy_recorder.session_id, &resource, 0..1)
                .await
            {
                Ok(Some(bytes)) if bytes.len() == 1 => {
                    total_length = Some(ready.total_length);
                    initial_probe = Some((Bytes::from(bytes), !was_cached));
                }
                Ok(_) => {
                    policy_recorder.record_upstream_failure(variant_id);
                    return text_response(
                        StatusCode::BAD_GATEWAY,
                        "HLS upstream returned an invalid initial byte range.\n",
                        false,
                    );
                }
                Err(error) => {
                    policy_recorder.record_upstream_failure(variant_id);
                    return hls_range_error_response(error);
                }
            },
            Err(HlsRangeError::QuotaExceeded) => {
                return proxy_hls_media_resource(
                    state,
                    policy_recorder,
                    variant_id,
                    resource,
                    headers,
                    false,
                )
                .await;
            }
            Err(HlsRangeError::RangeUnsupported) => {
                return proxy_hls_media_resource(
                    state,
                    policy_recorder,
                    variant_id,
                    resource,
                    headers,
                    false,
                )
                .await;
            }
            Err(error) => {
                policy_recorder.record_upstream_failure(variant_id);
                return hls_range_error_response(error);
            }
        }
    }

    let total_length = total_length.expect("range discovery should provide a total length");
    let probe_was_fetched = initial_probe.as_ref().is_some_and(|(_, fetched)| *fetched);
    let requested = match parse_range(range_header.as_ref(), total_length) {
        Ok(range) => range,
        Err(()) => return hls_range_not_satisfiable_response(Some(total_length)),
    };
    let (status, start, end, content_range) = match requested {
        Some(range) => (
            StatusCode::PARTIAL_CONTENT,
            range.start,
            range.end.saturating_add(1),
            Some(format!(
                "bytes {}-{}/{}",
                range.start, range.end, total_length
            )),
        ),
        None => (StatusCode::OK, 0, total_length, None),
    };
    if start == end {
        return hls_cached_range_response(resource, status, start..end, content_range, None);
    }

    let first_end = start.saturating_add(HLS_MEDIA_STREAM_CHUNK_BYTES).min(end);
    let first = if start == 0
        && first_end == 1
        && let Some(probe) = initial_probe.take()
    {
        Some(probe)
    } else {
        match read_or_fill_hls_range(
            state,
            &policy_recorder.session_id,
            &resource,
            start..first_end,
            &control,
        )
        .await
        {
            Ok((bytes, fetched, _)) => Some((bytes, fetched)),
            Err(HlsRangeError::QuotaExceeded) => {
                return proxy_hls_media_resource(
                    state,
                    policy_recorder,
                    variant_id,
                    resource,
                    headers,
                    false,
                )
                .await;
            }
            Err(HlsRangeError::RangeUnsupported) => {
                return proxy_hls_media_resource(
                    state,
                    policy_recorder,
                    variant_id,
                    resource,
                    headers,
                    false,
                )
                .await;
            }
            Err(error) => {
                policy_recorder.record_upstream_failure(variant_id);
                return hls_range_error_response(error);
            }
        }
    };
    let (first_bytes, first_was_fetched) = first.expect("nonempty response has a first chunk");
    let first_was_fetched = first_was_fetched || probe_was_fetched;
    if u64::try_from(first_bytes.len()).ok() != Some(first_end - start) {
        policy_recorder.record_upstream_failure(variant_id);
        return text_response(
            StatusCode::BAD_GATEWAY,
            "HLS range cache returned an inconsistent byte count.\n",
            false,
        );
    }

    let body = hls_cached_range_stream(HlsCachedRangeStreamState {
        media_state: state.clone(),
        policy_recorder: policy_recorder.clone(),
        variant_id: variant_id.to_owned(),
        resource: resource.clone(),
        session_id: policy_recorder.session_id.clone(),
        control,
        offset: first_end,
        end,
        total_length,
        first_bytes: Some(first_bytes),
        first_was_fetched,
        upstream_tail: None,
        observation: HlsRangePolicyObservation::new(
            policy_recorder.clone(),
            variant_id,
            end - start,
            first_was_fetched,
        ),
    });
    hls_cached_range_response(
        resource,
        status,
        start..end,
        content_range,
        Some(Body::from_stream(body)),
    )
}

fn hls_range_control(
    state: Arc<AppState>,
    session_id: String,
    generation: u64,
    expected_resource: HlsMediaResource,
) -> impl Fn() -> HlsCacheFillControl + Send + Sync {
    move || {
        let Some(current) = state.hls_sessions.get_with_generation(&session_id) else {
            return HlsCacheFillControl::Cancel;
        };
        if current.generation == generation {
            HlsCacheFillControl::Continue
        } else {
            let Some(authorized) = state.hls_playback_session_for_serving(&session_id) else {
                return HlsCacheFillControl::Cancel;
            };
            if authorized.generation != current.generation
                || authorized.session.advertise_alternate_variants
            {
                return HlsCacheFillControl::Cancel;
            }
            let Some((_, current_resource)) = authorized
                .session
                .media_resource_with_variant(&expected_resource.id)
            else {
                return HlsCacheFillControl::Cancel;
            };
            if same_hls_range_resource_representation(&expected_resource, &current_resource)
                && state
                    .hls_sessions
                    .get_with_generation(&session_id)
                    .is_some_and(|latest| latest.generation == authorized.generation)
            {
                HlsCacheFillControl::Continue
            } else {
                HlsCacheFillControl::Cancel
            }
        }
    }
}

fn same_hls_range_resource_representation(
    expected: &HlsMediaResource,
    current: &HlsMediaResource,
) -> bool {
    let expected_request = &expected.request;
    let current_request = &current.request;
    expected.id == current.id
        && expected_request.cache_key == current_request.cache_key
        && expected_request.kind == current_request.kind
        && expected_request.stream_id == current_request.stream_id
        && expected_request.codecs == current_request.codecs
        && expected_request.mime_type == current_request.mime_type
        && expected_request.bandwidth == current_request.bandwidth
        && expected_request.width == current_request.width
        && expected_request.height == current_request.height
        && expected_request.frame_rate == current_request.frame_rate
        && expected_request.duration_seconds == current_request.duration_seconds
        && expected_request
            .size
            .is_none_or(|expected_size| current_request.size == Some(expected_size))
}

async fn read_or_fill_hls_range<F>(
    state: &MediaState,
    session_id: &str,
    resource: &HlsMediaResource,
    requested: StdRange<u64>,
    control: &F,
) -> Result<(Bytes, bool, u64), HlsRangeError>
where
    F: Fn() -> HlsCacheFillControl + Send + Sync,
{
    if let Some(bytes) = state
        .state
        .hls_cache
        .read_resource_range(session_id, resource, requested.clone())
        .await?
    {
        if u64::try_from(bytes.len()).ok() != Some(requested.end - requested.start) {
            return Err(HlsRangeError::InvalidResponse(
                "HLS range cache returned an inconsistent byte count".to_owned(),
            ));
        }
        return Ok((
            Bytes::from(bytes),
            false,
            resource.request.size.unwrap_or(requested.end),
        ));
    }

    let ready = ensure_hls_range_with_media_refresh(
        state,
        session_id,
        resource,
        requested.clone(),
        HlsRangePriority::Foreground,
        control,
    )
    .await?;
    let bytes = state
        .state
        .hls_cache
        .read_resource_range(session_id, resource, requested.clone())
        .await?
        .ok_or_else(|| {
            HlsRangeError::InvalidResponse(
                "HLS range became non-durable after foreground completion".to_owned(),
            )
        })?;
    if u64::try_from(bytes.len()).ok() != Some(requested.end - requested.start) {
        return Err(HlsRangeError::InvalidResponse(
            "HLS range cache returned an inconsistent byte count".to_owned(),
        ));
    }
    Ok((Bytes::from(bytes), true, ready.total_length))
}

async fn ensure_hls_range_with_media_refresh<F>(
    state: &MediaState,
    session_id: &str,
    resource: &HlsMediaResource,
    requested: StdRange<u64>,
    priority: HlsRangePriority,
    control: &F,
) -> Result<HlsReadyRange, HlsRangeError>
where
    F: Fn() -> HlsCacheFillControl + Send + Sync,
{
    let failed_session = state.state.hls_sessions.get(session_id);
    let effective_resource = failed_session
        .as_ref()
        .and_then(|session| session.media_resource_with_variant(&resource.id))
        .map(|(_, current)| current)
        .filter(|current| same_hls_range_resource_representation(resource, current))
        .unwrap_or_else(|| resource.clone());
    let result = state
        .state
        .hls_cache
        .ensure_resource_range(
            &state.state.hls_upstream_client,
            session_id,
            &effective_resource,
            requested.clone(),
            priority,
            control,
        )
        .await;
    let error = match result {
        Err(error) if is_expired_range_error(&error) => error,
        result => return result,
    };
    let Some(failed_session) = failed_session else {
        return Err(error);
    };
    let refreshed = match state
        .state
        .refresh_hls_media_requests_for_resource(&failed_session, &effective_resource, control)
        .await
    {
        Ok(refreshed) => refreshed,
        Err(refresh_error) => return Err(refresh_error.into_range_error(error)),
    };
    let (_, replacement) = refreshed
        .media_resource_with_variant(&resource.id)
        .filter(|(_, replacement)| same_hls_range_resource_representation(resource, replacement))
        .ok_or(HlsRangeError::IdentityChanged)?;
    state
        .state
        .hls_cache
        .ensure_resource_range(
            &state.state.hls_upstream_client,
            session_id,
            &replacement,
            requested,
            priority,
            control,
        )
        .await
}

fn range_header_is_well_formed(header: &HeaderValue) -> bool {
    let Ok(value) = header.to_str() else {
        return false;
    };
    let Some(spec) = value.strip_prefix("bytes=") else {
        return false;
    };
    if spec.contains(',') {
        return false;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return false;
    };
    if start.is_empty() {
        return end.parse::<u64>().is_ok_and(|length| length > 0);
    }
    start.parse::<u64>().is_ok() && (end.is_empty() || end.parse::<u64>().is_ok())
}

fn hls_range_not_satisfiable_response(size: Option<u64>) -> Response<Body> {
    let size = size.map_or_else(|| "*".to_owned(), |size| size.to_string());
    let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
    response.headers_mut().insert(
        CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{size}"))
            .expect("content-range header should be valid"),
    );
    response
}

fn hls_range_error_response(error: HlsRangeError) -> Response<Body> {
    match error {
        HlsRangeError::QuotaExceeded => empty_response(StatusCode::INSUFFICIENT_STORAGE),
        HlsRangeError::Preempted => {
            let mut response = empty_response(StatusCode::SERVICE_UNAVAILABLE);
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
        HlsRangeError::Cancelled => {
            log_hls_media_not_found(HlsMediaNotFoundBranch::RangeCancelled);
            empty_response(StatusCode::NOT_FOUND)
        }
        HlsRangeError::SessionRemoving => {
            log_hls_media_not_found(HlsMediaNotFoundBranch::RangeSessionRemoving);
            empty_response(StatusCode::NOT_FOUND)
        }
        HlsRangeError::Io(error) => {
            eprintln!("HLS range cache read failed: {error}");
            empty_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
        HlsRangeError::Network(_) => {
            eprintln!("HLS foreground range source unavailable");
            empty_response(StatusCode::BAD_GATEWAY)
        }
        HlsRangeError::UpstreamStatus(status) => {
            eprintln!("HLS foreground range upstream returned {status}");
            empty_response(StatusCode::BAD_GATEWAY)
        }
        HlsRangeError::RangeUnsupported => text_response(
            StatusCode::BAD_GATEWAY,
            "HLS source does not support the requested byte range.\n",
            false,
        ),
        HlsRangeError::InvalidResponse(_) => {
            eprintln!("HLS foreground range response rejected");
            empty_response(StatusCode::BAD_GATEWAY)
        }
        HlsRangeError::IdentityChanged => {
            eprintln!("HLS foreground range representation changed");
            empty_response(StatusCode::BAD_GATEWAY)
        }
    }
}

fn hls_cached_range_response(
    resource: HlsMediaResource,
    status: StatusCode,
    requested: StdRange<u64>,
    content_range: Option<String>,
    body: Option<Body>,
) -> Response<Body> {
    let length = requested.end - requested.start;
    let mut builder = Response::builder()
        .status(status)
        .header(
            CONTENT_TYPE,
            content_type_header_value(resource.content_type()),
        )
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, length.to_string())
        .header(CACHE_CONTROL, "no-store");
    if let Some(content_range) = content_range {
        builder = builder.header(CONTENT_RANGE, content_range);
    }
    builder
        .body(body.unwrap_or_else(Body::empty))
        .expect("HLS cached range response should build")
}

struct HlsCachedRangeStreamState<F> {
    media_state: MediaState,
    policy_recorder: HlsNetworkPolicyRecorder,
    variant_id: String,
    resource: HlsMediaResource,
    session_id: String,
    control: F,
    offset: u64,
    end: u64,
    total_length: u64,
    first_bytes: Option<Bytes>,
    first_was_fetched: bool,
    upstream_tail: Option<BoxStream<'static, Result<Bytes, BoxError>>>,
    observation: HlsRangePolicyObservation,
}

fn hls_cached_range_stream<F>(
    state: HlsCachedRangeStreamState<F>,
) -> impl futures_core::Stream<Item = Result<Bytes, BoxError>>
where
    F: Fn() -> HlsCacheFillControl + Send + Sync + 'static,
{
    futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(tail) = state.upstream_tail.as_mut() {
                return match tail.next().await {
                    Some(Ok(bytes)) => Some((Ok(bytes), state)),
                    Some(Err(error)) => Some((Err(error), state)),
                    None => None,
                };
            }

            if let Some(bytes) = state.first_bytes.take() {
                state.observation.observe(&bytes, state.first_was_fetched);
                return Some((Ok(bytes), state));
            }
            if state.offset >= state.end {
                state.observation.finish();
                return None;
            }

            let next_end = state
                .offset
                .saturating_add(HLS_MEDIA_STREAM_CHUNK_BYTES)
                .min(state.end);
            let requested = state.offset..next_end;
            match read_or_fill_hls_range(
                &state.media_state,
                &state.session_id,
                &state.resource,
                requested,
                &state.control,
            )
            .await
            {
                Ok((bytes, fetched, _)) => {
                    if u64::try_from(bytes.len()).ok() != Some(next_end - state.offset) {
                        state
                            .policy_recorder
                            .record_upstream_failure(&state.variant_id);
                        state.observation.fail();
                        return Some((
                            Err(Box::new(std::io::Error::other(
                                "HLS range cache returned an inconsistent byte count",
                            ))),
                            state,
                        ));
                    }
                    state.offset = next_end;
                    state.observation.observe(&bytes, fetched);
                    return Some((Ok(bytes), state));
                }
                Err(HlsRangeError::QuotaExceeded) => {
                    let tail_end = state.end.checked_sub(1)?;
                    let Ok(range_header) =
                        HeaderValue::from_str(&format!("bytes={}-{}", state.offset, tail_end))
                    else {
                        state.observation.fail();
                        return Some((
                            Err(Box::new(std::io::Error::other(
                                "fallback HLS range header is invalid",
                            ))),
                            state,
                        ));
                    };
                    let mut tail_headers = HeaderMap::new();
                    tail_headers.insert(RANGE, range_header);
                    let response = proxy_hls_media_resource(
                        &state.media_state,
                        &state.policy_recorder,
                        &state.variant_id,
                        state.resource.clone(),
                        &tail_headers,
                        false,
                    )
                    .await;
                    let expected_content_range =
                        format!("bytes {}-{}/{}", state.offset, tail_end, state.total_length);
                    if response.status() != StatusCode::PARTIAL_CONTENT
                        || response
                            .headers()
                            .get(CONTENT_RANGE)
                            .and_then(|value| value.to_str().ok())
                            != Some(expected_content_range.as_str())
                    {
                        state.observation.delegate_to_proxy();
                        return Some((
                            Err(Box::new(std::io::Error::other(
                                "non-caching HLS range fallback failed validation",
                            ))),
                            state,
                        ));
                    }
                    state.upstream_tail = Some(
                        response
                            .into_body()
                            .into_data_stream()
                            .map_err(|error| -> BoxError { Box::new(error) })
                            .boxed(),
                    );
                    state.observation.delegate_to_proxy();
                }
                Err(error) => {
                    state
                        .policy_recorder
                        .record_upstream_failure(&state.variant_id);
                    state.observation.fail();
                    return Some((Err(Box::new(error)), state));
                }
            }
        }
    })
}

struct HlsRangePolicyObservation {
    policy_recorder: HlsNetworkPolicyRecorder,
    variant_id: String,
    expected_bytes: u64,
    emitted_bytes: u64,
    fetched: bool,
    cache_hit: bool,
    started_at: Instant,
    finished: bool,
    delegated: bool,
}

impl HlsRangePolicyObservation {
    fn new(
        policy_recorder: HlsNetworkPolicyRecorder,
        variant_id: &str,
        expected_bytes: u64,
        first_was_fetched: bool,
    ) -> Self {
        Self {
            policy_recorder,
            variant_id: variant_id.to_owned(),
            expected_bytes,
            emitted_bytes: 0,
            fetched: first_was_fetched,
            cache_hit: !first_was_fetched,
            started_at: Instant::now(),
            finished: false,
            delegated: false,
        }
    }

    fn observe(&mut self, bytes: &[u8], fetched: bool) {
        self.emitted_bytes = self
            .emitted_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.fetched |= fetched;
        self.cache_hit |= !fetched;
    }

    fn delegate_to_proxy(&mut self) {
        self.delegated = true;
    }

    fn fail(&mut self) {
        if !self.finished && !self.delegated {
            self.policy_recorder
                .record_upstream_failure(&self.variant_id);
            self.finished = true;
        }
    }

    fn finish(&mut self) {
        if self.finished || self.delegated || self.emitted_bytes != self.expected_bytes {
            return;
        }
        if self.fetched {
            self.policy_recorder
                .record_upstream_success(&self.variant_id, self.started_at.elapsed());
        } else if self.cache_hit {
            self.policy_recorder.record_cache_hit();
        }
        self.finished = true;
    }
}

impl Drop for HlsRangePolicyObservation {
    fn drop(&mut self) {
        self.finish();
    }
}

fn resource_has_upstream(resource: &HlsMediaResource) -> bool {
    !resource.request.url.trim().is_empty()
        || resource
            .request
            .backup_urls
            .iter()
            .any(|url| !url.trim().is_empty())
}

async fn proxy_hls_media_resource(
    state: &MediaState,
    policy_recorder: &HlsNetworkPolicyRecorder,
    variant_id: &str,
    resource: HlsMediaResource,
    headers: &HeaderMap,
    head_only: bool,
) -> Response<Body> {
    let failed_session = state.state.hls_sessions.get(&policy_recorder.session_id);
    let current_resource = failed_session
        .as_ref()
        .and_then(|session| session.media_resource_with_variant(&resource.id))
        .map(|(_, current)| current)
        .filter(|current| same_hls_range_resource_representation(&resource, current))
        .unwrap_or_else(|| resource.clone());
    let attempt = proxy_hls_media_resource_once(
        state,
        policy_recorder,
        variant_id,
        current_resource.clone(),
        headers,
        head_only,
    )
    .await;
    let Some(expired_status) = attempt.expired_status else {
        return attempt.response;
    };
    let Some(failed_session) = failed_session else {
        return attempt.response;
    };
    let control = hls_range_control(
        Arc::clone(&state.state),
        policy_recorder.session_id.clone(),
        policy_recorder.generation,
        current_resource.clone(),
    );
    let refreshed = match state
        .state
        .refresh_hls_media_requests_for_resource(&failed_session, &current_resource, &control)
        .await
    {
        Ok(refreshed) => refreshed,
        Err(crate::hls_media_refresh::HlsMediaRefreshError::Unavailable) => {
            return attempt.response;
        }
        Err(
            refresh_error @ (crate::hls_media_refresh::HlsMediaRefreshError::Cancelled
            | crate::hls_media_refresh::HlsMediaRefreshError::Preempted),
        ) => {
            return hls_range_error_response(
                refresh_error.into_range_error(HlsRangeError::UpstreamStatus(expired_status)),
            );
        }
        Err(refresh_error) => {
            if head_only {
                return attempt.response;
            }
            return hls_range_error_response(
                refresh_error.into_range_error(HlsRangeError::UpstreamStatus(expired_status)),
            );
        }
    };
    let Some((_, replacement)) =
        refreshed
            .media_resource_with_variant(&resource.id)
            .filter(|(_, replacement)| {
                same_hls_range_resource_representation(&current_resource, replacement)
            })
    else {
        if head_only {
            return attempt.response;
        }
        return hls_range_error_response(HlsRangeError::IdentityChanged);
    };
    proxy_hls_media_resource_once(
        state,
        policy_recorder,
        variant_id,
        replacement,
        headers,
        head_only,
    )
    .await
    .response
}

struct HlsProxyAttempt {
    response: Response<Body>,
    expired_status: Option<StatusCode>,
}

async fn proxy_hls_media_resource_once(
    state: &MediaState,
    policy_recorder: &HlsNetworkPolicyRecorder,
    variant_id: &str,
    resource: HlsMediaResource,
    headers: &HeaderMap,
    head_only: bool,
) -> HlsProxyAttempt {
    let urls = state.state.cdn_history.rank_request(&resource.request);
    let context = HlsMediaProxyContext {
        state,
        variant_id,
        resource: &resource,
        policy_recorder,
    };

    let mut last_retryable_response = None;
    let mut last_expired_response = None;
    for url in urls {
        match send_hls_upstream_request(context, &url, headers, head_only).await {
            Ok(upstream) if upstream.range_unsupported => {
                let mut fallback_headers = headers.clone();
                fallback_headers.remove(RANGE);
                match send_hls_upstream_request(context, &url, &fallback_headers, head_only).await {
                    Ok(fallback)
                        if should_retry_hls_upstream_status(fallback.response.status()) =>
                    {
                        policy_recorder.record_upstream_retry(variant_id);
                        if is_expired_media_status(fallback.response.status()) {
                            last_expired_response = Some(fallback.response);
                        } else {
                            last_retryable_response = Some(fallback.response);
                        }
                    }
                    Ok(fallback) if fallback.response.status() != StatusCode::OK => {
                        policy_recorder.record_upstream_failure(variant_id);
                        return HlsProxyAttempt {
                            response: text_response(
                                StatusCode::BAD_GATEWAY,
                                "HLS upstream did not provide a complete response after Range fallback.\n",
                                head_only,
                            ),
                            expired_status: None,
                        };
                    }
                    Ok(fallback) => {
                        return HlsProxyAttempt {
                            response: fallback.response,
                            expired_status: None,
                        };
                    }
                    Err(error) => {
                        if let Some(error_url) = error.url()
                            && let Some(observation_url) =
                                observation_candidate_url(&resource.request, error_url.as_str())
                        {
                            record_cdn_observation(
                                &state.state.cdn_history,
                                &resource.request,
                                &observation_url,
                                playback_observation(
                                    cdn_outcome_for_transport(&error),
                                    0,
                                    None,
                                    None,
                                    None,
                                ),
                            );
                        }
                        policy_recorder.record_upstream_retry(variant_id);
                        continue;
                    }
                }
            }
            Ok(upstream) if should_retry_hls_upstream_status(upstream.response.status()) => {
                policy_recorder.record_upstream_retry(variant_id);
                if is_expired_media_status(upstream.response.status()) {
                    last_expired_response = Some(upstream.response);
                } else {
                    last_retryable_response = Some(upstream.response);
                }
            }
            Ok(upstream) => {
                return HlsProxyAttempt {
                    response: upstream.response,
                    expired_status: None,
                };
            }
            Err(error) => {
                if let Some(error_url) = error.url()
                    && let Some(observation_url) =
                        observation_candidate_url(&resource.request, error_url.as_str())
                {
                    record_cdn_observation(
                        &state.state.cdn_history,
                        &resource.request,
                        &observation_url,
                        playback_observation(
                            cdn_outcome_for_transport(&error),
                            0,
                            None,
                            None,
                            None,
                        ),
                    );
                }
                policy_recorder.record_upstream_retry(variant_id);
                continue;
            }
        }
    }

    policy_recorder.record_upstream_failure(variant_id);
    if let Some(expired_response) = last_expired_response {
        let expired_status = expired_response.status();
        HlsProxyAttempt {
            response: expired_response,
            expired_status: Some(expired_status),
        }
    } else {
        HlsProxyAttempt {
            response: last_retryable_response.unwrap_or_else(|| {
                text_response(
                    StatusCode::BAD_GATEWAY,
                    "HLS upstream media request failed.\n",
                    head_only,
                )
            }),
            expired_status: None,
        }
    }
}

fn text_response(status: StatusCode, body: &'static str, head_only: bool) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(if head_only {
            Body::empty()
        } else {
            Body::from(body)
        })
        .expect("text response should build")
}

async fn send_hls_upstream_request(
    context: HlsMediaProxyContext<'_>,
    url: &str,
    headers: &HeaderMap,
    head_only: bool,
) -> Result<HlsUpstreamResponse, reqwest::Error> {
    let method = if head_only { Method::HEAD } else { Method::GET };
    let mut request = hls_upstream_request_builder(context.state, context.resource, method, url);
    if let Some(range) = headers.get(RANGE)
        && let Ok(range) = range.to_str()
    {
        request = request.header(RANGE.as_str(), range);
    }

    let started_at = Instant::now();
    let upstream = request.send().await?;
    let response_time = started_at.elapsed();
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let observation_url =
        observation_candidate_url(&context.resource.request, upstream.url().as_str());
    let upstream_headers = upstream.headers().clone();
    if headers.contains_key(RANGE) && status == StatusCode::OK {
        if let Some(observation_url) = observation_url.as_deref() {
            record_cdn_observation(
                &context.state.state.cdn_history,
                &context.resource.request,
                observation_url,
                playback_observation(CdnObservationOutcome::Partial, 0, None, None, Some(false)),
            );
        }
        return Ok(HlsUpstreamResponse {
            response: empty_response(StatusCode::OK),
            range_unsupported: true,
        });
    }
    if range_response_invalid(
        headers.get(RANGE),
        status,
        &upstream_headers,
        context.resource.request.size,
    ) {
        if let Some(observation_url) = observation_url.as_deref() {
            record_cdn_observation(
                &context.state.state.cdn_history,
                &context.resource.request,
                observation_url,
                playback_observation(
                    CdnObservationOutcome::IntegrityMismatch,
                    0,
                    None,
                    None,
                    Some(status == StatusCode::PARTIAL_CONTENT),
                ),
            );
        }
        return Ok(HlsUpstreamResponse {
            response: text_response(
                StatusCode::BAD_GATEWAY,
                "HLS upstream ignored byte range request.\n",
                head_only,
            ),
            range_unsupported: false,
        });
    }

    let expected_body_bytes = expected_media_body_bytes(
        &upstream_headers,
        status,
        headers.get(RANGE),
        context.resource.request.size,
    );
    let declared_body_bytes = upstream_headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let content_length_validated = declared_body_bytes
        .zip(expected_body_bytes)
        .is_some_and(|(declared, expected)| declared == expected);
    let body_length_invalid = headers.get(RANGE).is_some() && expected_body_bytes.is_none()
        || declared_body_bytes
            .zip(expected_body_bytes)
            .is_some_and(|(declared, expected)| declared != expected);
    if status.is_success() && body_length_invalid {
        if let Some(observation_url) = observation_url.as_deref() {
            record_cdn_observation(
                &context.state.state.cdn_history,
                &context.resource.request,
                observation_url,
                playback_observation(
                    CdnObservationOutcome::IntegrityMismatch,
                    0,
                    None,
                    None,
                    Some(status == StatusCode::PARTIAL_CONTENT),
                ),
            );
        }
        if !head_only {
            return Ok(HlsUpstreamResponse {
                response: text_response(
                    StatusCode::BAD_GATEWAY,
                    "HLS upstream returned inconsistent byte-range headers.\n",
                    head_only,
                ),
                range_unsupported: false,
            });
        }
    }

    if !status.is_success()
        && let Some(observation_url) = observation_url.as_deref()
    {
        record_cdn_observation(
            &context.state.state.cdn_history,
            &context.resource.request,
            observation_url,
            playback_observation(cdn_outcome_for_status(status), 0, None, None, None),
        );
    }

    let range_supported = headers
        .get(RANGE)
        .map(|_| status == StatusCode::PARTIAL_CONTENT);

    if head_only
        && status.is_success()
        && !body_length_invalid
        && let Some(observation_url) = observation_url.as_deref()
    {
        record_cdn_observation(
            &context.state.state.cdn_history,
            &context.resource.request,
            observation_url,
            playback_observation(
                CdnObservationOutcome::Complete,
                0,
                None,
                None,
                range_supported,
            ),
        );
    }

    let mut response = Response::builder()
        .status(status)
        .body(if head_only {
            if status.is_success() && !body_length_invalid {
                context
                    .policy_recorder
                    .record_upstream_success(context.variant_id, response_time);
            }
            Body::empty()
        } else if status.is_success() {
            hls_policy_recording_body(
                upstream,
                context.policy_recorder.clone(),
                context.variant_id.to_owned(),
                response_time,
                CdnBodyObservation::new(
                    Arc::clone(&context.state.state.cdn_history),
                    context.resource.request.clone(),
                    observation_url,
                    started_at,
                    range_supported,
                    expected_body_bytes,
                    content_length_validated,
                ),
            )
        } else {
            Body::from_stream(upstream.bytes_stream())
        })
        .expect("HLS upstream response should build");

    copy_hls_upstream_headers(
        &upstream_headers,
        response.headers_mut(),
        context.resource.content_type(),
    );

    Ok(HlsUpstreamResponse {
        response,
        range_unsupported: false,
    })
}

struct HlsUpstreamResponse {
    response: Response<Body>,
    range_unsupported: bool,
}

fn hls_policy_recording_body(
    upstream: reqwest::Response,
    policy_recorder: HlsNetworkPolicyRecorder,
    variant_id: String,
    response_time: Duration,
    observation: CdnBodyObservation,
) -> Body {
    Body::from_stream(hls_policy_recording_stream(
        upstream,
        policy_recorder,
        variant_id,
        response_time,
        observation,
    ))
}

fn hls_policy_recording_stream(
    upstream: reqwest::Response,
    policy_recorder: HlsNetworkPolicyRecorder,
    variant_id: String,
    response_time: Duration,
    observation: CdnBodyObservation,
) -> impl futures_core::Stream<Item = Result<Bytes, reqwest::Error>> {
    let stream = Box::pin(upstream.bytes_stream());
    futures_util::stream::unfold(
        (
            stream,
            policy_recorder,
            variant_id,
            response_time,
            observation,
            false,
        ),
        |(mut stream, policy_recorder, variant_id, response_time, mut observation, failed)| async move {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    observation.observe_chunk(&bytes);
                    Some((
                        Ok::<_, reqwest::Error>(bytes),
                        (
                            stream,
                            policy_recorder,
                            variant_id,
                            response_time,
                            observation,
                            failed,
                        ),
                    ))
                }
                Some(Err(error)) => {
                    if !failed {
                        policy_recorder.record_upstream_failure(&variant_id);
                        observation.finish(cdn_outcome_for_transport(&error));
                    }
                    Some((
                        Err(error),
                        (
                            stream,
                            policy_recorder,
                            variant_id,
                            response_time,
                            observation,
                            true,
                        ),
                    ))
                }
                None => {
                    if !failed {
                        let outcome = observation.finish(CdnObservationOutcome::Complete);
                        if outcome == CdnObservationOutcome::Complete {
                            policy_recorder.record_upstream_success(&variant_id, response_time);
                        } else {
                            policy_recorder.record_upstream_failure(&variant_id);
                        }
                    }
                    None
                }
            }
        },
    )
}

struct CdnBodyObservation {
    history: Arc<crate::cdn_history::CdnHistory>,
    request: crate::bbdown_adapter::BilibiliMediaRequest,
    url: Option<String>,
    started_at: Instant,
    range_supported: Option<bool>,
    expected_body_bytes: Option<u64>,
    content_length_validated: bool,
    bytes: u64,
    first_byte_latency: Option<Duration>,
    runtime: Option<tokio::runtime::Handle>,
    finished: bool,
}

impl CdnBodyObservation {
    fn new(
        history: Arc<crate::cdn_history::CdnHistory>,
        request: crate::bbdown_adapter::BilibiliMediaRequest,
        url: Option<String>,
        started_at: Instant,
        range_supported: Option<bool>,
        expected_body_bytes: Option<u64>,
        content_length_validated: bool,
    ) -> Self {
        Self {
            history,
            request,
            url,
            started_at,
            range_supported,
            expected_body_bytes,
            content_length_validated,
            bytes: 0,
            first_byte_latency: None,
            runtime: tokio::runtime::Handle::try_current().ok(),
            finished: false,
        }
    }

    fn observe_chunk(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        self.first_byte_latency
            .get_or_insert_with(|| self.started_at.elapsed());
    }

    fn finish(&mut self, outcome: CdnObservationOutcome) -> CdnObservationOutcome {
        if self.finished {
            return outcome;
        }
        let outcome = if outcome == CdnObservationOutcome::Complete
            && self
                .expected_body_bytes
                .is_some_and(|expected| expected != self.bytes)
        {
            CdnObservationOutcome::IntegrityMismatch
        } else {
            outcome
        };
        if let Some(url) = self.url.as_deref() {
            record_cdn_observation(
                &self.history,
                &self.request,
                url,
                playback_observation(
                    outcome,
                    self.bytes,
                    Some(self.started_at.elapsed()),
                    self.first_byte_latency,
                    self.range_supported,
                ),
            );
        }
        self.finished = true;
        outcome
    }
}

impl Drop for CdnBodyObservation {
    fn drop(&mut self) {
        if self.finished || (self.bytes == 0 && !self.content_length_validated) {
            return;
        }
        let (Some(runtime), Some(url)) = (self.runtime.take(), self.url.take()) else {
            return;
        };
        let history = Arc::clone(&self.history);
        let request = self.request.clone();
        let bytes = self.bytes;
        let elapsed = self.started_at.elapsed();
        let first_byte_latency = self.first_byte_latency;
        let range_supported = self.range_supported;
        let outcome = match self.expected_body_bytes {
            Some(expected) if self.bytes > expected => CdnObservationOutcome::IntegrityMismatch,
            Some(expected) if self.content_length_validated && self.bytes == expected => {
                CdnObservationOutcome::Complete
            }
            _ => CdnObservationOutcome::Partial,
        };
        runtime.spawn_blocking(move || {
            record_cdn_observation(
                &history,
                &request,
                &url,
                playback_observation(
                    outcome,
                    bytes,
                    Some(elapsed),
                    first_byte_latency,
                    range_supported,
                ),
            );
        });
    }
}

fn record_cdn_observation(
    history: &crate::cdn_history::CdnHistory,
    request: &crate::bbdown_adapter::BilibiliMediaRequest,
    url: &str,
    observation: CdnObservation,
) {
    history.record_request(request, url, observation);
}

fn playback_observation(
    outcome: CdnObservationOutcome,
    bytes: u64,
    elapsed: Option<Duration>,
    first_byte_latency: Option<Duration>,
    range_supported: Option<bool>,
) -> CdnObservation {
    CdnObservation {
        source: CdnObservationSource::Playback,
        outcome,
        bytes,
        elapsed,
        first_byte_latency,
        range_supported,
    }
}

fn cdn_outcome_for_status(status: StatusCode) -> CdnObservationOutcome {
    if status.is_server_error() {
        CdnObservationOutcome::ServerFailure
    } else if status == StatusCode::REQUEST_TIMEOUT || status == StatusCode::GATEWAY_TIMEOUT {
        CdnObservationOutcome::Timeout
    } else {
        CdnObservationOutcome::SourceUnavailable
    }
}

fn cdn_outcome_for_transport(error: &reqwest::Error) -> CdnObservationOutcome {
    if error.is_timeout() {
        CdnObservationOutcome::Timeout
    } else if error.is_connect() {
        CdnObservationOutcome::ConnectionFailure
    } else if error.is_body() || error.is_decode() {
        CdnObservationOutcome::IntegrityMismatch
    } else {
        CdnObservationOutcome::SourceUnavailable
    }
}

fn expected_media_body_bytes(
    headers: &HeaderMap,
    status: StatusCode,
    requested_range: Option<&HeaderValue>,
    known_total_length: Option<u64>,
) -> Option<u64> {
    let declared_length = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(requested_range) = requested_range {
        let (returned_range, total_length) = content_range_byte_range(headers)?;
        if status != StatusCode::PARTIAL_CONTENT
            || known_total_length.is_some_and(|known| known != total_length)
            || parse_range(Some(requested_range), total_length)
                .ok()
                .flatten()?
                != returned_range
        {
            return None;
        }
        let expected = returned_range.length();
        return Some(expected);
    }
    known_total_length.or(declared_length)
}

fn observation_candidate_url(
    request: &crate::bbdown_adapter::BilibiliMediaRequest,
    final_url: &str,
) -> Option<String> {
    let final_origin = reqwest::Url::parse(final_url).ok()?.origin();
    std::iter::once(request.url.as_str())
        .chain(request.backup_urls.iter().map(String::as_str))
        .find(|candidate| {
            reqwest::Url::parse(candidate)
                .ok()
                .is_some_and(|candidate| candidate.origin() == final_origin)
        })
        .map(str::to_owned)
}

fn hls_upstream_request_builder(
    state: &MediaState,
    resource: &HlsMediaResource,
    method: Method,
    url: &str,
) -> reqwest::RequestBuilder {
    let mut request = state.state.hls_upstream_client.request(method, url);
    for header in &resource.request.headers {
        if !should_forward_media_request_header(&header.name, &resource.request.url, url) {
            continue;
        }
        request = request.header(header.name.as_str(), header.value.as_str());
    }
    request
}

fn should_retry_hls_upstream_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::NOT_FOUND
                | StatusCode::GONE
                | StatusCode::TOO_MANY_REQUESTS
        )
}

fn is_expired_media_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE
    )
}

fn range_response_invalid(
    requested_range: Option<&HeaderValue>,
    status: StatusCode,
    headers: &HeaderMap,
    expected_total_length: Option<u64>,
) -> bool {
    let Some(requested_range) = requested_range else {
        return false;
    };
    if !status.is_success() {
        return false;
    }
    if status != StatusCode::PARTIAL_CONTENT {
        return true;
    }

    let Some((returned_range, total_length)) = content_range_byte_range(headers) else {
        return true;
    };
    if expected_total_length.is_some_and(|expected| expected != total_length) {
        return true;
    }
    let Ok(Some(expected_range)) = parse_range(Some(requested_range), total_length) else {
        return true;
    };

    returned_range != expected_range
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Mp4Initialization {
    length: u64,
    total_length: u64,
    segments: Vec<HlsMediaSegment>,
}

struct Mp4InitializationProbe {
    initialization: Mp4Initialization,
    response_time: Duration,
    prefix_bytes: u64,
    final_url: String,
}

async fn load_hls_mp4_initialization(
    state: &MediaState,
    policy_recorder: &HlsNetworkPolicyRecorder,
    variant_id: &str,
    resource: &HlsMediaResource,
) -> Result<Mp4Initialization, ()> {
    let failed_session = state.state.hls_sessions.get(&policy_recorder.session_id);
    let current_resource = failed_session
        .as_ref()
        .and_then(|session| session.media_resource_with_variant(&resource.id))
        .map(|(_, current)| current)
        .filter(|current| same_hls_range_resource_representation(resource, current))
        .unwrap_or_else(|| resource.clone());
    let (result, expired_status) = probe_hls_mp4_initialization_candidates(
        state,
        policy_recorder,
        variant_id,
        &current_resource,
    )
    .await;
    if result.is_ok() || expired_status.is_none() {
        return result;
    }
    let Some(failed_session) = failed_session else {
        return Err(());
    };
    let control = hls_range_control(
        Arc::clone(&state.state),
        policy_recorder.session_id.clone(),
        policy_recorder.generation,
        current_resource.clone(),
    );
    let refreshed = state
        .state
        .refresh_hls_media_requests_for_resource(&failed_session, &current_resource, &control)
        .await
        .map_err(|_| ())?;
    let (_, replacement) = refreshed
        .media_resource_with_variant(&resource.id)
        .filter(|(_, replacement)| {
            same_hls_range_resource_representation(&current_resource, replacement)
        })
        .ok_or(())?;
    let (result, _) =
        probe_hls_mp4_initialization_candidates(state, policy_recorder, variant_id, &replacement)
            .await;
    result
}

async fn probe_hls_mp4_initialization_candidates(
    state: &MediaState,
    policy_recorder: &HlsNetworkPolicyRecorder,
    variant_id: &str,
    resource: &HlsMediaResource,
) -> (Result<Mp4Initialization, ()>, Option<StatusCode>) {
    let urls = state
        .state
        .cdn_history
        .rank_request(&resource.request)
        .into_iter()
        .filter(|url| !url.trim().is_empty())
        .collect::<Vec<_>>();
    let mut expired_status = None;

    for url in urls {
        match load_hls_mp4_initialization_from_url(state, resource, &url).await {
            Ok(probe) => {
                if let Some(observation_url) =
                    observation_candidate_url(&resource.request, &probe.final_url)
                {
                    state.state.cdn_history.record_request(
                        &resource.request,
                        &observation_url,
                        CdnObservation {
                            source: CdnObservationSource::Playback,
                            outcome: CdnObservationOutcome::Partial,
                            bytes: probe.prefix_bytes,
                            elapsed: None,
                            first_byte_latency: None,
                            range_supported: Some(true),
                        },
                    );
                }
                policy_recorder.record_upstream_success(variant_id, probe.response_time);
                return (Ok(probe.initialization), expired_status);
            }
            Err(Mp4InitializationProbeError::Expired(status)) => {
                expired_status = Some(status);
                policy_recorder.record_upstream_retry(variant_id);
            }
            Err(Mp4InitializationProbeError::Other) => {
                policy_recorder.record_upstream_retry(variant_id);
            }
        }
    }

    (Err(()), expired_status)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mp4InitializationProbeError {
    Expired(StatusCode),
    Other,
}

async fn load_hls_mp4_initialization_from_url(
    state: &MediaState,
    resource: &HlsMediaResource,
    url: &str,
) -> Result<Mp4InitializationProbe, Mp4InitializationProbeError> {
    let started_at = Instant::now();
    let upstream = hls_upstream_request_builder(state, resource, Method::GET, url)
        .header(
            RANGE,
            format!("bytes=0-{}", HLS_INITIALIZATION_SCAN_BYTES - 1),
        )
        .send()
        .await
        .map_err(|_| Mp4InitializationProbeError::Other)?;
    let response_time = started_at.elapsed();
    let final_url = upstream.url().as_str().to_owned();
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .map_err(|_| Mp4InitializationProbeError::Other)?;
    if is_expired_media_status(status) {
        return Err(Mp4InitializationProbeError::Expired(status));
    }
    if should_retry_hls_upstream_status(status) || !status.is_success() {
        return Err(Mp4InitializationProbeError::Other);
    }

    let headers = upstream.headers().clone();
    let content_length = upstream.content_length();
    if status != StatusCode::PARTIAL_CONTENT
        || content_length.is_some_and(|length| length > HLS_INITIALIZATION_SCAN_BYTES)
    {
        return Err(Mp4InitializationProbeError::Other);
    }
    let (returned_range, total_length) =
        content_range_byte_range(&headers).ok_or(Mp4InitializationProbeError::Other)?;
    if returned_range.start != 0 || returned_range.length() > HLS_INITIALIZATION_SCAN_BYTES {
        return Err(Mp4InitializationProbeError::Other);
    }
    let bytes = read_hls_initialization_probe(upstream)
        .await
        .map_err(|_| Mp4InitializationProbeError::Other)?;
    let length = mp4_initialization_length(&bytes).ok_or(Mp4InitializationProbeError::Other)?;
    if length == 0 || length >= total_length {
        return Err(Mp4InitializationProbeError::Other);
    }

    Ok(Mp4InitializationProbe {
        initialization: Mp4Initialization {
            length,
            total_length,
            segments: Vec::new(),
        },
        response_time,
        prefix_bytes: u64::try_from(bytes.len()).map_err(|_| Mp4InitializationProbeError::Other)?,
        final_url,
    })
}

async fn read_hls_initialization_probe(upstream: reqwest::Response) -> Result<Vec<u8>, ()> {
    let max_bytes = usize::try_from(HLS_INITIALIZATION_SCAN_BYTES).map_err(|_| ())?;
    let mut bytes = Vec::new();
    let mut stream = upstream.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        let next_len = bytes.len().checked_add(chunk.len()).ok_or(())?;
        if next_len > max_bytes {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

fn content_range_byte_range(headers: &HeaderMap) -> Option<(ByteRange, u64)> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let spec = value.strip_prefix("bytes ")?;
    let (range, total) = spec.rsplit_once('/')?;
    if total == "*" {
        return None;
    }
    let total = total.parse().ok()?;
    let (start, end) = range.split_once('-')?;
    let range = ByteRange {
        start: start.parse().ok()?,
        end: end.parse().ok()?,
    };
    if total == 0 || range.start > range.end || range.end >= total {
        return None;
    }

    Some((range, total))
}

fn copy_hls_upstream_headers(
    source: &HeaderMap,
    target: &mut HeaderMap,
    fallback_content_type: &str,
) {
    for name in [
        ACCEPT_RANGES,
        CACHE_CONTROL,
        CONTENT_LENGTH,
        CONTENT_RANGE,
        ETAG,
        LAST_MODIFIED,
    ] {
        if let Some(value) = source.get(&name) {
            target.insert(name, value.clone());
        }
    }

    let content_type = source
        .get(CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| content_type_header_value(fallback_content_type));
    target.insert(CONTENT_TYPE, content_type);
}

fn content_type_header_value(value: &str) -> HeaderValue {
    if value.is_empty() {
        return HeaderValue::from_static("application/octet-stream");
    }
    HeaderValue::from_str(value)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

fn resource_not_found_response() -> Response<Body> {
    let mut response = empty_response(StatusCode::NOT_FOUND);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

fn resource_open_busy_response() -> Response<Body> {
    let mut response = empty_response(StatusCode::SERVICE_UNAVAILABLE);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

fn resource_range_not_satisfiable_response(
    size: u64,
    content_type: &str,
    supports_byte_ranges: bool,
    etag: &str,
) -> Response<Body> {
    let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
    response.headers_mut().insert(
        CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{size}"))
            .expect("content range header should be valid"),
    );
    response
        .headers_mut()
        .insert(CONTENT_TYPE, content_type_header_value(content_type));
    apply_resource_headers(response.headers_mut(), supports_byte_ranges, etag);
    response
}

fn apply_resource_headers(headers: &mut HeaderMap, supports_byte_ranges: bool, etag: &str) {
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    headers.insert(
        ACCEPT_RANGES,
        if supports_byte_ranges {
            HeaderValue::from_static("bytes")
        } else {
            HeaderValue::from_static("none")
        },
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Some(etag) = quoted_etag_header_value(etag) {
        headers.insert(ETAG, etag);
    }
}

fn quoted_etag_header_value(value: &str) -> Option<HeaderValue> {
    if value.is_empty() {
        return None;
    }
    let opaque = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    if opaque.is_empty()
        || !opaque
            .bytes()
            .all(|byte| byte == b'!' || matches!(byte, b'#'..=b'~'))
    {
        return None;
    }
    HeaderValue::from_str(&format!("\"{opaque}\"")).ok()
}

fn resource_is_not_modified(
    headers: &HeaderMap,
    etag: Option<&HeaderValue>,
    last_modified: std::time::SystemTime,
) -> bool {
    if headers.contains_key(IF_NONE_MATCH) {
        return headers
            .get_all(IF_NONE_MATCH)
            .iter()
            .any(|value| if_none_match_field_matches(value, etag));
    }
    headers
        .get(IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .is_some_and(|date| is_not_modified_since(last_modified, date))
}

fn if_none_match_field_matches(value: &HeaderValue, etag: Option<&HeaderValue>) -> bool {
    let Ok(value) = value.to_str() else {
        return false;
    };
    let current = etag.and_then(|etag| etag.to_str().ok());
    let bytes = value.as_bytes();
    let mut offset = skip_optional_whitespace(bytes, 0);
    if bytes.get(offset) == Some(&b'*') {
        offset = skip_optional_whitespace(bytes, offset + 1);
        return offset == bytes.len();
    }

    let mut matched = false;
    loop {
        offset = skip_optional_whitespace(bytes, offset);
        let candidate_start = offset;
        if bytes.get(offset..offset + 2) == Some(b"W/") {
            offset += 2;
        }
        if bytes.get(offset) != Some(&b'"') {
            return false;
        }
        offset += 1;
        while let Some(byte) = bytes.get(offset) {
            if *byte == b'"' {
                break;
            }
            if *byte != b'!' && !matches!(*byte, b'#'..=b'~') {
                return false;
            }
            offset += 1;
        }
        if bytes.get(offset) != Some(&b'"') {
            return false;
        }
        offset += 1;
        let candidate = &value[candidate_start..offset];
        matched |=
            current.is_some_and(|current| weak_etag_value(candidate) == weak_etag_value(current));
        offset = skip_optional_whitespace(bytes, offset);
        if offset == bytes.len() {
            return matched;
        }
        if bytes.get(offset) != Some(&b',') {
            return false;
        }
        offset += 1;
    }
}

fn skip_optional_whitespace(value: &[u8], mut offset: usize) -> usize {
    while matches!(value.get(offset), Some(b' ' | b'\t')) {
        offset += 1;
    }
    offset
}

fn weak_etag_value(value: &str) -> &str {
    value.strip_prefix("W/").unwrap_or(value)
}

fn single_range_header(headers: &HeaderMap) -> Result<Option<&HeaderValue>, ()> {
    let mut values = headers.get_all(RANGE).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }

    Ok(first)
}

fn range_validator_matches(
    headers: &HeaderMap,
    etag: Option<&HeaderValue>,
    last_modified: std::time::SystemTime,
) -> bool {
    let Some(if_range) = headers.get(IF_RANGE) else {
        return true;
    };
    let Ok(if_range) = if_range.to_str() else {
        return false;
    };
    if if_range.starts_with('"') || if_range.starts_with("W/\"") {
        return !if_range.starts_with("W/")
            && etag
                .and_then(|etag| etag.to_str().ok())
                .is_some_and(|etag| etag == if_range);
    }
    httpdate::parse_http_date(if_range)
        .ok()
        .is_some_and(|date| is_not_modified_since(last_modified, date))
}

fn is_not_modified_since(
    last_modified: std::time::SystemTime,
    comparison: std::time::SystemTime,
) -> bool {
    let Ok(last_modified) = last_modified.duration_since(std::time::SystemTime::UNIX_EPOCH) else {
        return false;
    };
    let Ok(comparison) = comparison.duration_since(std::time::SystemTime::UNIX_EPOCH) else {
        return false;
    };
    last_modified.as_secs() <= comparison.as_secs()
}

fn resource_not_modified_response(
    content_type: &str,
    supports_byte_ranges: bool,
    etag: Option<HeaderValue>,
    last_modified: std::time::SystemTime,
) -> Response<Body> {
    let mut response = empty_response(StatusCode::NOT_MODIFIED);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, content_type_header_value(content_type));
    response.headers_mut().insert(
        LAST_MODIFIED,
        HeaderValue::from_str(&httpdate::fmt_http_date(last_modified))
            .expect("last-modified header should be valid"),
    );
    apply_resource_headers(response.headers_mut(), supports_byte_ranges, "");
    if let Some(etag) = etag {
        response.headers_mut().insert(ETAG, etag);
    }
    response
}

async fn build_file_response(
    opened_file: OpenedMediaFile,
    range: Option<ByteRange>,
    head_only: bool,
) -> Response<Body> {
    let size = opened_file.size_bytes;
    let (status, start, length, content_range) = if let Some(range) = range {
        (
            StatusCode::PARTIAL_CONTENT,
            range.start,
            range.length(),
            Some(format!("bytes {}-{}/{}", range.start, range.end, size)),
        )
    } else {
        (StatusCode::OK, 0, size, None)
    };

    let body = if head_only {
        Body::empty()
    } else {
        let mut file = tokio::fs::File::from_std(opened_file.file);
        if file.seek(SeekFrom::Start(start)).await.is_err() {
            return empty_response(StatusCode::NOT_FOUND);
        }

        Body::from_stream(ReaderStream::new(file.take(length)))
    };

    let mut response = Response::builder()
        .status(status)
        .header(
            CONTENT_TYPE,
            content_type_header_value(&opened_file.content_type),
        )
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, length.to_string())
        .header(
            LAST_MODIFIED,
            httpdate::fmt_http_date(opened_file.last_modified),
        )
        .body(body)
        .expect("media response should build");

    if let Some(content_range) = content_range {
        response.headers_mut().insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&content_range).expect("content range header should be valid"),
        );
    }

    response
}

async fn build_prewarmed_file_response(
    opened_file: OpenedPrewarmedHlsResource,
    range: ByteRange,
    head_only: bool,
) -> Response<Body> {
    let body = if head_only {
        Body::empty()
    } else {
        let mut file = tokio::fs::File::from_std(opened_file.file);
        if file.seek(SeekFrom::Start(range.start)).await.is_err() {
            return empty_response(StatusCode::NOT_FOUND);
        }

        Body::from_stream(ReaderStream::new(file.take(range.length())))
    };

    let mut response = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(
            CONTENT_TYPE,
            content_type_header_value(&opened_file.content_type),
        )
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, range.length().to_string())
        .header(
            CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                range.start, range.end, opened_file.total_length
            ),
        )
        .header(
            LAST_MODIFIED,
            httpdate::fmt_http_date(opened_file.last_modified),
        )
        .body(body)
        .expect("prewarmed media response should build");
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Clone, Copy)]
struct HlsMediaProxyContext<'a> {
    state: &'a MediaState,
    variant_id: &'a str,
    resource: &'a HlsMediaResource,
    policy_recorder: &'a HlsNetworkPolicyRecorder,
}

async fn build_prewarmed_spliced_file_response(
    context: HlsMediaProxyContext<'_>,
    opened_file: OpenedPrewarmedHlsResource,
    range: ByteRange,
    headers: &HeaderMap,
    head_only: bool,
) -> Response<Body> {
    if head_only {
        return proxy_hls_media_resource(
            context.state,
            context.policy_recorder,
            context.variant_id,
            context.resource.clone(),
            headers,
            true,
        )
        .await;
    }

    let tail = match open_hls_upstream_tail_response(
        context,
        opened_file.prefix_length,
        range.end,
        opened_file.total_length,
    )
    .await
    {
        Ok(tail) => tail,
        Err(()) => {
            return proxy_hls_media_resource(
                context.state,
                context.policy_recorder,
                context.variant_id,
                context.resource.clone(),
                headers,
                false,
            )
            .await;
        }
    };

    let local_length = opened_file.prefix_length - range.start;
    let body = {
        let mut file = tokio::fs::File::from_std(opened_file.file);
        if file.seek(SeekFrom::Start(range.start)).await.is_err() {
            return empty_response(StatusCode::NOT_FOUND);
        }
        let local_stream = ReaderStream::new(file.take(local_length))
            .map_err(|error| -> BoxError { Box::new(error) });
        let upstream_stream = hls_policy_recording_stream(
            tail.upstream,
            context.policy_recorder.clone(),
            context.variant_id.to_owned(),
            tail.response_time,
            CdnBodyObservation::new(
                Arc::clone(&context.state.state.cdn_history),
                context.resource.request.clone(),
                observation_candidate_url(&context.resource.request, &tail.final_url),
                tail.started_at,
                Some(true),
                Some(tail.length),
                tail.content_length_validated,
            ),
        )
        .map_err(|error| -> BoxError { Box::new(error) });
        Body::from_stream(local_stream.chain(upstream_stream))
    };

    let mut response = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(
            CONTENT_TYPE,
            content_type_header_value(&opened_file.content_type),
        )
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, range.length().to_string())
        .header(
            CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                range.start, range.end, opened_file.total_length
            ),
        )
        .header(
            LAST_MODIFIED,
            httpdate::fmt_http_date(opened_file.last_modified),
        )
        .body(body)
        .expect("spliced prewarmed media response should build");
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

struct HlsUpstreamTailResponse {
    upstream: reqwest::Response,
    response_time: Duration,
    started_at: Instant,
    final_url: String,
    length: u64,
    content_length_validated: bool,
}

async fn open_hls_upstream_tail_response(
    context: HlsMediaProxyContext<'_>,
    start: u64,
    end: u64,
    total_length: u64,
) -> Result<HlsUpstreamTailResponse, ()> {
    let tail_range = HeaderValue::from_str(&format!("bytes={start}-{end}")).map_err(|_| ())?;
    let urls = context
        .state
        .state
        .cdn_history
        .rank_request(&context.resource.request)
        .into_iter()
        .filter(|url| !url.trim().is_empty())
        .collect::<Vec<_>>();

    for url in urls {
        let started_at = Instant::now();
        let upstream =
            match hls_upstream_request_builder(context.state, context.resource, Method::GET, &url)
                .header(RANGE.as_str(), tail_range.to_str().map_err(|_| ())?)
                .send()
                .await
            {
                Ok(upstream) => upstream,
                Err(error) => {
                    if let Some(error_url) = error.url()
                        && let Some(observation_url) =
                            observation_candidate_url(&context.resource.request, error_url.as_str())
                    {
                        record_cdn_observation(
                            &context.state.state.cdn_history,
                            &context.resource.request,
                            &observation_url,
                            playback_observation(
                                cdn_outcome_for_transport(&error),
                                0,
                                None,
                                None,
                                None,
                            ),
                        );
                    }
                    context
                        .policy_recorder
                        .record_upstream_retry(context.variant_id);
                    continue;
                }
            };
        let response_time = started_at.elapsed();
        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let final_url = upstream.url().as_str().to_owned();
        let observation_url = observation_candidate_url(&context.resource.request, &final_url);
        if should_retry_hls_upstream_status(status) || !status.is_success() {
            if let Some(observation_url) = observation_url.as_deref() {
                record_cdn_observation(
                    &context.state.state.cdn_history,
                    &context.resource.request,
                    observation_url,
                    playback_observation(cdn_outcome_for_status(status), 0, None, None, None),
                );
            }
            context
                .policy_recorder
                .record_upstream_retry(context.variant_id);
            continue;
        }
        let headers = upstream.headers();
        let declared_body_bytes = upstream.content_length();
        let Some((returned_range, returned_total_length)) = content_range_byte_range(headers)
        else {
            if let Some(observation_url) = observation_url.as_deref() {
                record_cdn_observation(
                    &context.state.state.cdn_history,
                    &context.resource.request,
                    observation_url,
                    playback_observation(
                        CdnObservationOutcome::IntegrityMismatch,
                        0,
                        None,
                        None,
                        Some(false),
                    ),
                );
            }
            context
                .policy_recorder
                .record_upstream_retry(context.variant_id);
            continue;
        };
        let expected_range = ByteRange { start, end };
        if status != StatusCode::PARTIAL_CONTENT
            || returned_range != expected_range
            || returned_total_length != total_length
            || declared_body_bytes.is_some_and(|length| length != expected_range.length())
        {
            if let Some(observation_url) = observation_url.as_deref() {
                record_cdn_observation(
                    &context.state.state.cdn_history,
                    &context.resource.request,
                    observation_url,
                    playback_observation(
                        CdnObservationOutcome::IntegrityMismatch,
                        0,
                        None,
                        None,
                        Some(status == StatusCode::PARTIAL_CONTENT),
                    ),
                );
            }
            context
                .policy_recorder
                .record_upstream_retry(context.variant_id);
            continue;
        }

        return Ok(HlsUpstreamTailResponse {
            upstream,
            response_time,
            started_at,
            final_url,
            length: expected_range.length(),
            content_length_validated: declared_body_bytes
                .is_some_and(|length| length == expected_range.length()),
        });
    }

    Err(())
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("empty response should build")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    fn length(self) -> u64 {
        self.end - self.start + 1
    }
}

fn parse_range(header: Option<&HeaderValue>, size: u64) -> Result<Option<ByteRange>, ()> {
    let Some(header) = header else {
        return Ok(None);
    };
    let value = header.to_str().map_err(|_| ())?;
    let Some(spec) = value.strip_prefix("bytes=") else {
        return Err(());
    };
    if spec.contains(',') || size == 0 {
        return Err(());
    }

    let (start, end) = spec.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix_length = end.parse::<u64>().map_err(|_| ())?;
        if suffix_length == 0 {
            return Err(());
        }

        let start = size.saturating_sub(suffix_length);
        return Ok(Some(ByteRange {
            start,
            end: size - 1,
        }));
    }

    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }

    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(size - 1)
    };
    if end < start {
        return Err(());
    }

    Ok(Some(ByteRange { start, end }))
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, fs, path::PathBuf, sync::mpsc, thread};

    use super::*;
    use axum::{
        Router,
        body::{Bytes, to_bytes},
        routing::get,
    };
    use tempfile::TempDir;
    use tokio::{
        sync::{Notify, watch},
        task::JoinHandle,
    };

    use crate::{
        bbdown_adapter::{
            BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest,
            BilibiliMediaRequestKind,
        },
        cdn_history::CdnHistory,
        config::CacheServerOptions,
        generated::tvos_net_player::v1::{
            BilibiliPlaybackSession, BilibiliPlaybackVariant, BilibiliTaskResultItem,
            CacheResourceRef, PlaybackProtocol, PlaybackSource, TaskArtifact, TaskArtifactKind,
            TaskArtifactState, TaskResult, TaskState,
        },
        hls::{HlsMediaResource, HlsPlaybackSession, HlsVariant},
        task_output::TaskResourceRecord,
    };

    fn response_content_length(response: &Response<Body>) -> Option<u64> {
        response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
    }

    struct TaskResourceFixture {
        temp: TempDir,
        state: MediaState,
        resource_id: String,
        resource_path: PathBuf,
    }

    #[test]
    fn cdn_ranking_keeps_refreshed_candidate_urls_and_is_variant_scoped() {
        let history = CdnHistory::default();
        let mut request = media_request(
            "https://cdn-a.example/video.m4s?expires=1",
            vec!["https://cdn-b.example/video.m4s?expires=1".to_owned()],
        );
        history.record_request(
            &request,
            &request.backup_urls[0],
            CdnObservation::playback_complete(
                4096,
                Duration::from_millis(50),
                Duration::from_millis(8),
                Some(true),
            ),
        );

        request.url = "https://cdn-a.example/video.m4s?expires=2".to_owned();
        request.backup_urls[0] = "https://cdn-b.example/video.m4s?expires=2".to_owned();
        assert_eq!(
            request.backup_urls[0],
            history.rank_request(&request)[0],
            "history should rank the fresh URL without rewriting its query"
        );

        let mut other_variant = request.clone();
        other_variant.codecs = Some("hev1.1.6.L120.90".to_owned());
        other_variant.cache_key.codecs = other_variant.codecs.clone();
        other_variant.url = "https://variant-c.example/other.m4s?version=2".to_owned();
        other_variant.backup_urls.clear();
        assert_eq!(
            vec![other_variant.url.clone()],
            history.rank_request(&other_variant)
        );
    }

    #[test]
    fn cdn_conflicting_length_headers_do_not_disable_body_validation() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("3"));
        assert_eq!(
            expected_media_body_bytes(&headers, StatusCode::OK, None, Some(8)),
            Some(8)
        );
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 2-5/8"));
        let range = HeaderValue::from_static("bytes=2-5");
        assert_eq!(
            expected_media_body_bytes(&headers, StatusCode::PARTIAL_CONTENT, Some(&range), Some(8)),
            Some(4)
        );
    }

    #[test]
    fn cdn_redirected_observation_uses_only_a_matching_fresh_candidate_origin() {
        let request = media_request(
            "https://cdn-a.example/video.m4s?expires=2",
            vec!["https://cdn-b.example/video.m4s?expires=2".to_owned()],
        );

        assert_eq!(
            Some(request.backup_urls[0].clone()),
            observation_candidate_url(
                &request,
                "https://cdn-b.example/redirected/signed?expires=9"
            )
        );
        assert_eq!(
            None,
            observation_candidate_url(&request, "https://unlisted.example/redirected/video")
        );
    }

    #[test]
    fn cdn_body_integrity_observation_does_not_cool_down_shared_cdn_host() {
        let history = CdnHistory::default();
        let request = media_request(
            "https://cdn-a.example/video.m4s?expires=2",
            vec!["https://cdn-b.example/video.m4s?expires=2".to_owned()],
        );
        let mut observation = CdnBodyObservation::new(
            Arc::new(history.clone()),
            request.clone(),
            Some(request.url.clone()),
            Instant::now(),
            None,
            Some(10),
            false,
        );
        observation.observe_chunk(b"short");
        observation.finish(CdnObservationOutcome::Complete);
        assert_eq!(request.backup_urls[0], history.rank_request(&request)[0]);

        let mut unrelated = media_request(
            "https://cdn-a.example/other.m4s?expires=3",
            vec!["https://cdn-c.example/other.m4s?expires=3".to_owned()],
        );
        unrelated.cache_key.content_id = "other-content".to_owned();
        assert_eq!(
            CdnHistory::default().rank_request(&unrelated),
            history.rank_request(&unrelated)
        );
    }

    #[tokio::test]
    async fn fully_consumed_http_proxy_body_updates_shared_cdn_ranking() {
        let (upstream_url, _upstream_task) = start_hls_upstream().await;
        let backup_url = "http://127.0.0.1:9/backup.m4s".to_owned();
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session =
            hls_session_with_backups("session-1", &upstream_url, vec![backup_url.clone()]);
        session.variant.video.request.size = Some(b"video-data".len() as u64);
        insert_authorized_hls_session(&state, session);

        let mut ranking_request = media_request(&backup_url, vec![upstream_url.clone()]);
        ranking_request.size = Some(b"video-data".len() as u64);
        assert_eq!(
            vec![backup_url.clone(), upstream_url.clone()],
            state.cdn_history.rank_request(&ranking_request)
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy listener should bind");
        let proxy_address = listener.local_addr().unwrap();
        let proxy = Router::new()
            .route(
                "/hls/{session_id}/segments/{segment_id}",
                get(hls_segment_get),
            )
            .with_state(MediaState::new(state.clone()));
        let _proxy_task = tokio::spawn(async move {
            axum::serve(listener, proxy)
                .await
                .expect("proxy server should run");
        });

        let response = reqwest::Client::new()
            .get(format!(
                "http://{proxy_address}/hls/session-1/segments/video.m4s"
            ))
            .send()
            .await
            .expect("proxied HTTP response should arrive");
        assert_eq!(StatusCode::OK, response.status());
        assert_eq!(Some(10), response.content_length());
        assert_eq!(
            b"video-data",
            &response
                .bytes()
                .await
                .expect("proxied body should be fully consumed")[..]
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if state.cdn_history.rank_request(&ranking_request)[0] == upstream_url {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fully consumed ordinary body should update shared CDN ranking");
    }

    fn test_resource(id: &str, body: &[u8]) -> CacheResourceRef {
        CacheResourceRef {
            id: id.to_owned(),
            content_type: "text/vtt; charset=utf-8".to_owned(),
            size_bytes: body.len().try_into().unwrap(),
            size_known: true,
            supports_byte_ranges: true,
            etag: "resource-v1".to_owned(),
            ..Default::default()
        }
    }

    fn task_resource_fixture(
        resource: CacheResourceRef,
        body: Option<&[u8]>,
    ) -> TaskResourceFixture {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        fs::create_dir_all(&root_path).expect("cache root should be created");
        let root_path = root_path.canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: temp.path().join("state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let task = state
            .tasks
            .create_bilibili_task("BV1-resource-test", None)
            .expect("test task should be created");
        let record = TaskResourceRecord::new(resource).expect("test resource should be valid");
        let resource_id = record.resource.id.clone();
        let resource_path = root_path.join(record.relative_path());
        let publication_size: usize = record
            .resource
            .size_bytes
            .try_into()
            .expect("test resource size should fit in memory");
        let publication_body = body
            .filter(|body| body.len() == publication_size)
            .map_or_else(|| vec![0; publication_size], <[u8]>::to_vec);
        let results = vec![TaskResult {
            id: "result-one".to_owned(),
            state: TaskState::Completed.into(),
            artifacts: vec![TaskArtifact {
                id: "artifact-one".to_owned(),
                kind: TaskArtifactKind::Subtitle.into(),
                state: TaskArtifactState::Available.into(),
                resource: Some(record.resource.clone()),
                ..Default::default()
            }],
            ..Default::default()
        }];
        let staged = state
            .tasks
            .stage_task_output_replacement(&task.id, vec![record])
            .expect("test task output should stage");
        staged
            .write_resource_body(&resource_id, &publication_body)
            .expect("resource body should be written");
        staged
            .commit(results)
            .expect("test task output should be committed");
        match body {
            None => fs::remove_file(&resource_path)
                .expect("missing-body fixture should remove the published body"),
            Some(body) if body.len() != publication_size => {
                fs::remove_file(&resource_path)
                    .expect("mismatched-body fixture should remove the published body");
                fs::write(&resource_path, body)
                    .expect("mismatched-body fixture should install replacement bytes");
            }
            Some(_) => {}
        }

        TaskResourceFixture {
            temp,
            state: MediaState::new(state),
            resource_id,
            resource_path,
        }
    }

    async fn assert_path_free_not_found(response: Response<Body>, private_path: &std::path::Path) {
        assert_eq!(StatusCode::NOT_FOUND, response.status());
        let private_path = private_path.to_string_lossy();
        assert!(response.headers().values().all(|value| {
            value
                .to_str()
                .map(|value| !value.contains(private_path.as_ref()))
                .unwrap_or(true)
        }));
        assert_eq!(
            Some("nosniff"),
            response
                .headers()
                .get(&X_CONTENT_TYPE_OPTIONS)
                .and_then(|value| value.to_str().ok())
        );
        assert_eq!(
            Some("no-store"),
            response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|value| value.to_str().ok())
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());
    }

    async fn assert_path_free_unavailable(
        response: Response<Body>,
        private_path: &std::path::Path,
    ) {
        assert_eq!(StatusCode::SERVICE_UNAVAILABLE, response.status());
        let private_path = private_path.to_string_lossy();
        assert!(response.headers().values().all(|value| {
            value
                .to_str()
                .map(|value| !value.contains(private_path.as_ref()))
                .unwrap_or(true)
        }));
        assert_eq!("no-store", response.headers()[CACHE_CONTROL]);
        assert_eq!("1", response.headers()[RETRY_AFTER]);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());
    }

    fn assert_resource_requires_revalidation(response: &Response<Body>) {
        assert_eq!(
            Some("private, no-cache"),
            response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|value| value.to_str().ok())
        );
    }

    #[test]
    fn parses_standard_ranges() {
        assert_eq!(Some(ByteRange { start: 0, end: 3 }), parse("bytes=0-3", 16));
        assert_eq!(Some(ByteRange { start: 4, end: 15 }), parse("bytes=4-", 16));
        assert_eq!(
            Some(ByteRange { start: 12, end: 15 }),
            parse("bytes=-4", 16)
        );
    }

    #[test]
    fn rejects_unsatisfiable_ranges() {
        assert!(parse_range(Some(&HeaderValue::from_static("bytes=99-100")), 16).is_err());
        assert!(parse_range(Some(&HeaderValue::from_static("items=0-1")), 16).is_err());
        assert!(parse_range(Some(&HeaderValue::from_static("bytes=0-1,2-3")), 16).is_err());
    }

    #[tokio::test]
    async fn task_resource_get_streams_full_body_with_canonical_headers() {
        let body = b"0123456789abcdef";
        let fixture = task_resource_fixture(test_resource("resource-full", body), Some(body));

        let response = resource_get(
            State(fixture.state.clone()),
            Path(fixture.resource_id.clone()),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        assert_eq!("text/vtt; charset=utf-8", response.headers()[CONTENT_TYPE]);
        assert_eq!("16", response.headers()[CONTENT_LENGTH]);
        assert_eq!("bytes", response.headers()[ACCEPT_RANGES]);
        assert_eq!("\"resource-v1\"", response.headers()[ETAG]);
        assert_resource_requires_revalidation(&response);
        assert_eq!(
            Some("nosniff"),
            response
                .headers()
                .get(&X_CONTENT_TYPE_OPTIONS)
                .and_then(|value| value.to_str().ok())
        );
        assert_eq!(
            body.as_slice(),
            &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn task_resource_open_keeps_the_async_executor_responsive() {
        let body = b"blocking-pool";
        let fixture = task_resource_fixture(test_resource("resource-blocking", body), Some(body));
        let tasks = Arc::clone(&fixture.state.state.tasks);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            tasks.block_resource_cleanup_for_test(ready_tx, release_rx);
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup lock holder should start");

        let started = std::time::Instant::now();
        let request = tokio::spawn(resource_get(
            State(fixture.state.clone()),
            Path(fixture.resource_id.clone()),
            HeaderMap::new(),
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let executor_stayed_responsive = started.elapsed() < Duration::from_millis(500);

        let _ = release_tx.send(());
        holder.join().expect("cleanup lock holder should stop");
        assert!(
            executor_stayed_responsive,
            "resource authorization and open must run outside the async executor"
        );
        let response = tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .expect("resource request should finish after cleanup is released")
            .expect("resource request task should not panic");
        assert_eq!(StatusCode::OK, response.status());
    }

    #[tokio::test]
    async fn task_resource_head_returns_full_headers_without_a_body() {
        let body = b"0123456789abcdef";
        let fixture = task_resource_fixture(test_resource("resource-head", body), Some(body));

        for range in [None, Some("bytes=2-5"), Some("bytes=99-100")] {
            let mut headers = HeaderMap::new();
            if let Some(range) = range {
                headers.insert(RANGE, HeaderValue::from_static(range));
            }
            let response = resource_head(
                State(fixture.state.clone()),
                Path(fixture.resource_id.clone()),
                headers,
            )
            .await;

            assert_eq!(StatusCode::OK, response.status());
            assert_eq!("text/vtt; charset=utf-8", response.headers()[CONTENT_TYPE]);
            assert_eq!("16", response.headers()[CONTENT_LENGTH]);
            assert_eq!("bytes", response.headers()[ACCEPT_RANGES]);
            assert_eq!("\"resource-v1\"", response.headers()[ETAG]);
            assert!(!response.headers().contains_key(CONTENT_RANGE));
            assert_resource_requires_revalidation(&response);
            assert!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn task_resource_get_supports_normal_and_suffix_ranges() {
        let body = b"0123456789abcdef";
        let fixture = task_resource_fixture(test_resource("resource-range", body), Some(body));

        for (header, expected_range, expected_body) in [
            ("bytes=2-5", "bytes 2-5/16", &b"2345"[..]),
            ("bytes=-4", "bytes 12-15/16", &b"cdef"[..]),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RANGE, HeaderValue::from_static(header));

            let response = resource_get(
                State(fixture.state.clone()),
                Path(fixture.resource_id.clone()),
                headers,
            )
            .await;

            assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
            assert_eq!(expected_range, response.headers()[CONTENT_RANGE]);
            assert_eq!("4", response.headers()[CONTENT_LENGTH]);
            assert_resource_requires_revalidation(&response);
            assert_eq!(
                expected_body,
                &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..]
            );
        }
    }

    #[tokio::test]
    async fn task_resource_open_fails_fast_when_blocking_jobs_are_saturated() {
        let body = b"bounded";
        let fixture = task_resource_fixture(test_resource("resource-busy", body), Some(body));
        let permits = Arc::clone(&fixture.state.state.task_resource_open_permits);
        let permit_count: u32 = permits
            .available_permits()
            .try_into()
            .expect("permit count should fit in u32");
        let _held_permits = permits
            .acquire_many_owned(permit_count)
            .await
            .expect("all test permits should be available");

        let response = resource_get(
            State(fixture.state.clone()),
            Path(fixture.resource_id.clone()),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::SERVICE_UNAVAILABLE, response.status());
        assert_eq!("no-store", response.headers()[CACHE_CONTROL]);
        assert_eq!("1", response.headers()[RETRY_AFTER]);
    }

    #[tokio::test]
    async fn task_resource_get_rejects_repeated_range_header_fields() {
        let body = b"0123456789abcdef";
        let fixture =
            task_resource_fixture(test_resource("resource-repeated-range", body), Some(body));
        let mut headers = HeaderMap::new();
        headers.append(RANGE, HeaderValue::from_static("bytes=2-5"));
        headers.append(RANGE, HeaderValue::from_static("bytes=6-7"));

        let response = resource_get(
            State(fixture.state.clone()),
            Path(fixture.resource_id.clone()),
            headers,
        )
        .await;

        assert_eq!(StatusCode::RANGE_NOT_SATISFIABLE, response.status());
        assert_eq!("bytes */16", response.headers()[CONTENT_RANGE]);
        assert_eq!("text/vtt; charset=utf-8", response.headers()[CONTENT_TYPE]);
        assert_eq!("bytes", response.headers()[ACCEPT_RANGES]);
        assert_eq!("\"resource-v1\"", response.headers()[ETAG]);
        assert!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn task_resource_honors_conditional_get_validators() {
        let body = b"0123456789abcdef";
        let fixture =
            task_resource_fixture(test_resource("resource-conditional", body), Some(body));
        let baseline = resource_get(
            State(fixture.state.clone()),
            Path(fixture.resource_id.clone()),
            HeaderMap::new(),
        )
        .await;
        let last_modified = baseline.headers()[LAST_MODIFIED].clone();

        for (name, value) in [
            (IF_NONE_MATCH, HeaderValue::from_static("W/\"resource-v1\"")),
            (IF_MODIFIED_SINCE, last_modified),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(name, value);
            let response = resource_get(
                State(fixture.state.clone()),
                Path(fixture.resource_id.clone()),
                headers,
            )
            .await;

            assert_eq!(StatusCode::NOT_MODIFIED, response.status());
            assert_eq!("\"resource-v1\"", response.headers()[ETAG]);
            assert_resource_requires_revalidation(&response);
            assert!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn task_resource_matches_repeated_etags_with_quoted_commas() {
        let body = b"0123456789abcdef";
        let mut resource = test_resource("resource-repeated-etag", body);
        resource.etag = "part,1".to_owned();
        let fixture = task_resource_fixture(resource, Some(body));
        let mut headers = HeaderMap::new();
        headers.append(IF_NONE_MATCH, HeaderValue::from_static("\"stale\""));
        headers.append(
            IF_NONE_MATCH,
            HeaderValue::from_static("W/\"part,1\", \"other\""),
        );

        let response = resource_get(
            State(fixture.state.clone()),
            Path(fixture.resource_id.clone()),
            headers,
        )
        .await;

        assert_eq!(StatusCode::NOT_MODIFIED, response.status());
        assert_eq!("\"part,1\"", response.headers()[ETAG]);
        assert!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn task_resource_applies_ranges_only_when_if_range_matches() {
        let body = b"0123456789abcdef";
        let fixture = task_resource_fixture(test_resource("resource-if-range", body), Some(body));

        for (validator, expected_status, expected_body) in [
            ("\"resource-v1\"", StatusCode::PARTIAL_CONTENT, &b"2345"[..]),
            ("\"stale\"", StatusCode::OK, body.as_slice()),
            ("W/\"resource-v1\"", StatusCode::OK, body.as_slice()),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RANGE, HeaderValue::from_static("bytes=2-5"));
            headers.insert(IF_RANGE, HeaderValue::from_str(validator).unwrap());
            let response = resource_get(
                State(fixture.state.clone()),
                Path(fixture.resource_id.clone()),
                headers,
            )
            .await;

            assert_eq!(expected_status, response.status());
            assert_eq!(
                expected_body,
                &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..]
            );
        }
    }

    #[tokio::test]
    async fn task_resource_get_ignores_repeated_range_header_fields_when_if_range_mismatches() {
        let body = b"0123456789abcdef";
        let fixture = task_resource_fixture(
            test_resource("resource-if-range-repeated", body),
            Some(body),
        );
        let mut headers = HeaderMap::new();
        headers.append(RANGE, HeaderValue::from_static("bytes=2-5"));
        headers.append(RANGE, HeaderValue::from_static("bytes=6-7"));
        headers.insert(IF_RANGE, HeaderValue::from_static("\"stale\""));

        let response = resource_get(
            State(fixture.state.clone()),
            Path(fixture.resource_id.clone()),
            headers,
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        assert!(!response.headers().contains_key(CONTENT_RANGE));
        assert_eq!(
            body.as_slice(),
            &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..]
        );
    }

    #[tokio::test]
    async fn task_resource_rejects_invalid_and_multipart_ranges() {
        let body = b"0123456789abcdef";
        let fixture =
            task_resource_fixture(test_resource("resource-invalid-range", body), Some(body));

        for header in ["bytes=99-100", "bytes=0-1,4-5"] {
            let mut headers = HeaderMap::new();
            headers.insert(RANGE, HeaderValue::from_static(header));

            let response = resource_get(
                State(fixture.state.clone()),
                Path(fixture.resource_id.clone()),
                headers,
            )
            .await;

            assert_eq!(StatusCode::RANGE_NOT_SATISFIABLE, response.status());
            assert_eq!("bytes */16", response.headers()[CONTENT_RANGE]);
            assert!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn task_resource_error_responses_do_not_disclose_paths() {
        let body = b"0123456789abcdef";
        let valid = task_resource_fixture(test_resource("resource-valid", body), Some(body));
        assert_path_free_not_found(
            resource_get(
                State(valid.state.clone()),
                Path("unknown-resource".to_owned()),
                HeaderMap::new(),
            )
            .await,
            &valid.resource_path,
        )
        .await;

        let missing = task_resource_fixture(test_resource("resource-missing", body), None);
        assert_path_free_unavailable(
            resource_get(
                State(missing.state.clone()),
                Path(missing.resource_id.clone()),
                HeaderMap::new(),
            )
            .await,
            &missing.resource_path,
        )
        .await;

        let mut mismatched_resource = test_resource("resource-mismatch", body);
        mismatched_resource.size_bytes += 1;
        let mismatched = task_resource_fixture(mismatched_resource, Some(body));
        assert_path_free_unavailable(
            resource_get(
                State(mismatched.state.clone()),
                Path(mismatched.resource_id.clone()),
                HeaderMap::new(),
            )
            .await,
            &mismatched.resource_path,
        )
        .await;

        let mut expired_resource = test_resource("resource-expired", body);
        expired_resource.expires_at = Some(prost_types::Timestamp {
            seconds: 0,
            nanos: 0,
        });
        let expired = task_resource_fixture(expired_resource, Some(body));
        assert_path_free_not_found(
            resource_get(
                State(expired.state.clone()),
                Path(expired.resource_id.clone()),
                HeaderMap::new(),
            )
            .await,
            &expired.resource_path,
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_resource_refuses_symlink_targets() {
        use std::os::unix::fs::symlink;

        let body = b"0123456789abcdef";
        let fixture = task_resource_fixture(test_resource("resource-symlink", body), None);
        let outside_path = fixture.temp.path().join("outside-secret.txt");
        fs::write(&outside_path, b"secret resource contents").unwrap();
        symlink(&outside_path, &fixture.resource_path).unwrap();

        let response = resource_get(
            State(fixture.state.clone()),
            Path(fixture.resource_id.clone()),
            HeaderMap::new(),
        )
        .await;

        assert_path_free_unavailable(response, &outside_path).await;
    }

    #[tokio::test]
    async fn hls_segment_proxies_upstream_media_with_required_headers_and_range() {
        let (upstream_url, _upstream_task) = start_hls_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = Some(b"video-data".len() as u64);
        insert_authorized_hls_session(&state, session);
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!("video/mp4", response.headers()[CONTENT_TYPE]);
        assert_eq!("bytes 1-3/10", response.headers()[CONTENT_RANGE]);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(b"ide", &body[..]);
    }

    #[tokio::test]
    async fn hls_master_playlist_demotes_unhealthy_variant_from_network_policy() {
        let (upstream_url, _upstream_task) = start_hls_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        insert_authorized_hls_session(
            &state,
            hls_session_with_alternate("session-1", &upstream_url),
        );
        state
            .hls_network_policy
            .record_upstream_failure("session-1", "h264-1080p");

        let response =
            hls_master_playlist_get(State(MediaState::new(state)), Path("session-1".to_owned()))
                .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let master = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(1, master.matches("#EXT-X-STREAM-INF").count());
        assert!(master.contains("BANDWIDTH=600000"));
        assert!(master.contains("segments/v1-video.m3u8\n"));
        assert!(!master.contains("segments/video.m3u8\n"));
    }

    #[tokio::test]
    async fn hold_downgrade_rejects_stale_high_variant_urls_after_master_load() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session_with_alternate("session-1", &upstream_url);
        session.effective_policy.weak_network_preference = WeakNetworkPreference::HoldDowngrade;
        let generation = insert_authorized_hls_session(&state, session);

        let initial_master = hls_master_playlist_get(
            State(MediaState::new(state.clone())),
            Path("session-1".to_owned()),
        )
        .await;
        let initial_master = to_bytes(initial_master.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            2,
            String::from_utf8(initial_master.to_vec())
                .unwrap()
                .matches("#EXT-X-STREAM-INF")
                .count()
        );

        state.hls_network_policy.record_upstream_failure_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session-1",
            generation,
            "h264-1080p",
        );

        let stale_playlist = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(StatusCode::SERVICE_UNAVAILABLE, stale_playlist.status());

        let stale_segment = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(StatusCode::SERVICE_UNAVAILABLE, stale_segment.status());

        let lower_playlist = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "v1-video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(StatusCode::OK, lower_playlist.status());
    }

    #[tokio::test]
    async fn adaptive_downgrade_keeps_stale_high_variant_url_servable() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let generation = insert_authorized_hls_session(
            &state,
            hls_session_with_alternate("session-1", &upstream_url),
        );
        state.hls_network_policy.record_upstream_failure_for_policy(
            WeakNetworkPreference::Adaptive,
            "session-1",
            generation,
            "h264-1080p",
        );

        let stale_playlist = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, stale_playlist.status());
    }

    #[tokio::test]
    async fn hls_master_playlist_keeps_variants_when_avplayer_manages_network() {
        let (upstream_url, _upstream_task) = start_hls_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session_with_alternate("session-1", &upstream_url);
        session.effective_policy.weak_network_preference = WeakNetworkPreference::AvPlayerManaged;
        insert_authorized_hls_session(&state, session);
        state
            .hls_network_policy
            .record_upstream_failure("session-1", "h264-1080p");

        let response =
            hls_master_playlist_get(State(MediaState::new(state)), Path("session-1".to_owned()))
                .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let master = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(2, master.matches("#EXT-X-STREAM-INF").count());
        assert!(master.contains("segments/video.m3u8\n"));
        assert!(master.contains("segments/v1-video.m3u8\n"));
    }

    #[tokio::test]
    async fn cache_only_master_omits_partial_alternate_variant() {
        let (upstream_url, _upstream_task) = start_hls_large_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut cached_session = hls_session_with_alternate("session-1", &upstream_url);
        let total_length = large_fake_mp4().len() as u64;
        cached_session.variant.video.request.size = Some(total_length);
        cached_session.alternate_variants[0].video.request.size = Some(total_length);
        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &cached_session)
            .await
            .expect("selected representation should be cached");
        state
            .hls_cache
            .ensure_resource_range(
                &state.hls_upstream_client,
                &cached_session.id,
                &cached_session.alternate_variants[0].video,
                0..1,
                crate::hls_range_cache::HlsRangePriority::Foreground,
                &|| crate::hls_cache::HlsCacheFillControl::Continue,
            )
            .await
            .expect("alternate representation should have durable partial data");
        assert!(
            state
                .hls_cache
                .range_durable_bytes(
                    &cached_session.id,
                    &cached_session.alternate_variants[0].video,
                )
                .expect("partial range status should be readable")
                < total_length
        );

        let mut offline_session = cached_session;
        offline_session.variant.video.request.url.clear();
        offline_session.variant.video.request.backup_urls.clear();
        offline_session.alternate_variants[0]
            .video
            .request
            .url
            .clear();
        offline_session.alternate_variants[0]
            .video
            .request
            .backup_urls
            .clear();
        insert_authorized_hls_session(&state, offline_session);

        let response =
            hls_master_playlist_get(State(MediaState::new(state)), Path("session-1".to_owned()))
                .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let master = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(1, master.matches("#EXT-X-STREAM-INF").count());
        assert!(master.contains("segments/video.m3u8\n"));
        assert!(!master.contains("segments/v1-video.m3u8\n"));
    }

    #[tokio::test]
    async fn stale_hold_recorder_cannot_recreate_state_after_completion_or_removal() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session("session-completed", "https://example.test/video.m4s");
        let generation = state.hls_sessions.insert(session.clone());
        let media_state = MediaState::new(state.clone());
        let recorder = HlsNetworkPolicyRecorder::new(
            &media_state,
            session.id.clone(),
            generation,
            WeakNetworkPreference::HoldDowngrade,
        );
        recorder.record_upstream_failure("h264");
        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::UpstreamFailed,
            state.hls_weak_network_status().state
        );

        state.register_completed_hls_runtime_session(&session);
        recorder.record_upstream_failure("h264");
        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::Normal,
            state.hls_weak_network_status().state
        );

        let removed_session = hls_session("session-removed", "https://example.test/video.m4s");
        let removed_generation = state.hls_sessions.insert(removed_session.clone());
        let removed_recorder = HlsNetworkPolicyRecorder::new(
            &media_state,
            removed_session.id.clone(),
            removed_generation,
            WeakNetworkPreference::HoldDowngrade,
        );
        state.remove_hls_playback_session(&removed_session.id);
        removed_recorder.record_upstream_failure("h264");
        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::Normal,
            state.hls_weak_network_status().state
        );
    }

    #[test]
    fn network_policy_update_is_serialized_with_session_removal() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session("session-race", "https://example.test/video.m4s");
        let generation = state.register_hls_playback_session(session.clone());
        let (update_started_tx, update_started_rx) = mpsc::channel();
        let (release_update_tx, release_update_rx) = mpsc::channel();
        let update_state = state.clone();
        let session_id = session.id.clone();
        let updater = thread::spawn(move || {
            let updated = update_state.hls_sessions.with_network_policy_update(
                &session_id,
                generation,
                || {
                    update_started_tx.send(()).unwrap();
                    release_update_rx.recv().unwrap();
                    update_state
                        .hls_network_policy
                        .record_upstream_failure_for_policy(
                            WeakNetworkPreference::HoldDowngrade,
                            &session_id,
                            generation,
                            "h264",
                        );
                },
            );
            assert!(updated);
        });

        update_started_rx.recv().unwrap();
        let (removal_started_tx, removal_started_rx) = mpsc::channel();
        let (removal_done_tx, removal_done_rx) = mpsc::channel();
        let removal_state = state.clone();
        let removal_session_id = session.id.clone();
        let remover = thread::spawn(move || {
            removal_started_tx.send(()).unwrap();
            removal_state.remove_hls_playback_session(&removal_session_id);
            removal_done_tx.send(()).unwrap();
        });

        removal_started_rx.recv().unwrap();
        assert!(
            removal_done_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "session removal must wait for the in-flight policy update"
        );
        release_update_tx.send(()).unwrap();
        updater.join().unwrap();
        removal_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("session removal should finish after the policy update");
        remover.join().unwrap();

        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::Normal,
            state.hls_weak_network_status().state
        );
    }

    #[test]
    fn session_registration_is_serialized_with_session_removal() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session(
            "session-registration-race",
            "https://example.test/video.m4s",
        );
        let session_id = session.id.clone();
        let (registration_started_tx, registration_started_rx) = mpsc::channel();
        let (release_registration_tx, release_registration_rx) = mpsc::channel();
        let registration_state = state.clone();
        let registration_session_id = session_id.clone();
        let registrar = thread::spawn(move || {
            registration_state
                .hls_sessions
                .insert_with_generation_update(session, |generation| {
                    registration_started_tx.send(()).unwrap();
                    release_registration_rx.recv().unwrap();
                    registration_state
                        .hls_network_policy
                        .advance_session_generation(&registration_session_id, generation);
                })
        });

        registration_started_rx.recv().unwrap();
        let (removal_started_tx, removal_started_rx) = mpsc::channel();
        let (removal_done_tx, removal_done_rx) = mpsc::channel();
        let removal_state = state.clone();
        let removal_session_id = session_id.clone();
        let remover = thread::spawn(move || {
            removal_started_tx.send(()).unwrap();
            removal_state.remove_hls_playback_session(&removal_session_id);
            removal_done_tx.send(()).unwrap();
        });

        removal_started_rx.recv().unwrap();
        assert!(
            removal_done_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "session removal must wait for the registration policy update"
        );
        release_registration_tx.send(()).unwrap();
        registrar.join().unwrap();
        removal_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("session removal should finish after registration");
        remover.join().unwrap();

        assert!(state.hls_sessions.get(&session_id).is_none());
        assert_eq!(
            None,
            state
                .hls_network_policy
                .session_generation_for_tests(&session_id)
        );
    }

    #[tokio::test]
    async fn hls_media_playlist_uses_mp4_initialization_map_and_byte_range() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        insert_authorized_hls_session(&state, hls_session("session-1", &upstream_url));

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        assert_eq!(
            "application/vnd.apple.mpegurl",
            response.headers()[CONTENT_TYPE]
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let playlist = String::from_utf8(body.to_vec()).unwrap();
        let mp4 = fake_mp4();
        let initialization_length = mp4_initialization_length(&mp4).unwrap();
        let media_length = u64::try_from(mp4.len()).unwrap() - initialization_length;
        assert!(playlist.contains(&format!(
            "#EXT-X-MAP:URI=\"video.m4s\",BYTERANGE=\"{initialization_length}@0\""
        )));
        assert!(playlist.contains(&format!(
            "#EXT-X-BYTERANGE:{media_length}@{initialization_length}"
        )));
    }

    #[tokio::test]
    async fn hls_media_playlist_records_retry_when_initialization_probe_uses_backup() {
        let (primary_url, _primary_task) = start_hls_forbidden_upstream().await;
        let (backup_url, _backup_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        insert_authorized_hls_session(
            &state,
            hls_session_with_backups("session-1", &primary_url, vec![backup_url]),
        );

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        let snapshot = state.hls_network_policy.snapshot();
        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::Retrying,
            snapshot.state
        );
        assert_eq!(1, snapshot.retrying_variant_count);
    }

    #[tokio::test]
    async fn hls_media_playlist_avplayer_managed_mode_ignores_probe_retry_state() {
        let (primary_url, _primary_task) = start_hls_forbidden_upstream().await;
        let (backup_url, _backup_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session_with_backups("session-1", &primary_url, vec![backup_url]);
        session.effective_policy.weak_network_preference = WeakNetworkPreference::AvPlayerManaged;
        insert_authorized_hls_session(&state, session);

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        let snapshot = state.hls_network_policy.snapshot();
        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::Normal,
            snapshot.state
        );
        assert_eq!(0, snapshot.retrying_variant_count);
    }

    #[tokio::test]
    async fn hls_media_playlist_slow_initialization_body_uses_header_latency_for_policy() {
        let (upstream_url, _upstream_task) = start_hls_slow_initialization_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        insert_authorized_hls_session(&state, hls_session("session-1", &upstream_url));

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let playlist = String::from_utf8(body.to_vec()).unwrap();
        assert!(playlist.contains("#EXT-X-MAP:URI=\"video.m4s\""));
        let snapshot = state.hls_network_policy.snapshot();
        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::Normal,
            snapshot.state
        );
        assert_eq!(0, snapshot.retrying_variant_count);
        assert_eq!(0, snapshot.degraded_session_count);
    }

    #[tokio::test]
    async fn hls_media_playlist_uses_cached_initialization_without_upstream_probe() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let cached_session = hls_session("session-1", &upstream_url);
        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &cached_session)
            .await
            .expect("session should cache");
        insert_authorized_hls_session(
            &state,
            hls_session("session-1", "http://127.0.0.1:9/unreachable.m4s"),
        );

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let playlist = String::from_utf8(body.to_vec()).unwrap();
        assert!(playlist.contains("#EXT-X-MAP:URI=\"video.m4s\",BYTERANGE=\"28@0\""));
    }

    #[tokio::test]
    async fn hls_media_playlist_uses_prewarmed_initialization_without_upstream_probe() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = Some(fake_mp4().len() as u64);
        state
            .hls_cache
            .prewarm_session_first_frame_with_control(&state.hls_upstream_client, &session, || {
                crate::hls_cache::HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm");
        insert_authorized_hls_session(
            &state,
            hls_session("session-1", "http://127.0.0.1:9/unreachable.m4s"),
        );

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let playlist = String::from_utf8(body.to_vec()).unwrap();
        assert!(playlist.contains("#EXT-X-MAP:URI=\"video.m4s\",BYTERANGE=\"28@0\""));
    }

    #[tokio::test]
    async fn hls_media_playlist_keeps_full_media_range_for_prewarmed_prefix() {
        let (upstream_url, _upstream_task) = start_hls_large_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = Some(large_fake_mp4().len() as u64);
        session.variant.video.request.bandwidth = Some(0);
        state
            .hls_cache
            .prewarm_session_first_frame_with_control(&state.hls_upstream_client, &session, || {
                crate::hls_cache::HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm");
        let prewarmed = state
            .hls_cache
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("prewarm metadata should load");
        assert!(prewarmed.prefix_length < prewarmed.total_length);
        insert_authorized_hls_session(
            &state,
            hls_session("session-1", "http://127.0.0.1:9/unreachable.m4s"),
        );

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let playlist = String::from_utf8(body.to_vec()).unwrap();
        let initialization_length = prewarmed.initialization_length;
        let prefix_media_length = prewarmed.prefix_length - initialization_length;
        let full_media_length = prewarmed.total_length - initialization_length;
        assert!(playlist.contains(&format!(
            "#EXT-X-BYTERANGE:{full_media_length}@{initialization_length}"
        )));
        assert!(!playlist.contains(&format!("@{HLS_INITIALIZATION_SCAN_BYTES}")));

        let mut headers = HeaderMap::new();
        headers.insert(
            RANGE,
            HeaderValue::from_str(&format!(
                "bytes={initialization_length}-{}",
                prewarmed.prefix_length - 1
            ))
            .expect("range header should be valid"),
        );
        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!(
            format!(
                "bytes {initialization_length}-{}/{}",
                prewarmed.prefix_length - 1,
                prewarmed.total_length
            ),
            response.headers()[CONTENT_RANGE]
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(usize::try_from(prefix_media_length).unwrap(), body.len());
    }

    #[tokio::test]
    async fn hls_segment_splices_prewarmed_prefix_with_upstream_tail() {
        let (upstream_url, _upstream_task) = start_hls_large_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let backup_url = "https://backup.example/video.m4s".to_owned();
        let mut session =
            hls_session_with_backups("session-1", &upstream_url, vec![backup_url.clone()]);
        session.variant.video.request.size = Some(large_fake_mp4().len() as u64);
        session.variant.video.request.bandwidth = Some(0);
        state
            .hls_cache
            .prewarm_session_first_frame_with_control(&state.hls_upstream_client, &session, || {
                crate::hls_cache::HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm");
        let prewarmed = state
            .hls_cache
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("prewarm metadata should load");
        assert!(prewarmed.prefix_length < prewarmed.total_length);
        let request = session.variant.video.request.clone();
        state.cdn_history.record_request(
            &request,
            &upstream_url,
            CdnObservation::playback_complete(
                4096,
                Duration::from_millis(10),
                Duration::from_millis(1),
                Some(true),
            ),
        );
        assert_eq!(
            upstream_url,
            state.cdn_history.rank_request(&request)[0],
            "known-healthy primary should be selected ahead of an unknown backup"
        );
        insert_authorized_hls_session(&state, session);

        let initialization_length = prewarmed.initialization_length;
        let mut headers = HeaderMap::new();
        headers.insert(
            RANGE,
            HeaderValue::from_str(&format!(
                "bytes={initialization_length}-{}",
                prewarmed.total_length - 1
            ))
            .expect("range header should be valid"),
        );
        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!(
            format!(
                "bytes {initialization_length}-{}/{}",
                prewarmed.total_length - 1,
                prewarmed.total_length
            ),
            response.headers()[CONTENT_RANGE]
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            &large_fake_mp4()[usize::try_from(initialization_length).unwrap()..],
            &body[..]
        );
        assert_eq!(
            upstream_url,
            state.cdn_history.rank_request(&request)[0],
            "fully consumed valid tail should keep the primary ahead of the backup"
        );
        let snapshot = state.hls_network_policy.snapshot();
        assert_eq!(0, snapshot.retrying_variant_count);
        assert_eq!(0, snapshot.degraded_session_count);
    }

    #[tokio::test]
    async fn hls_segment_spliced_truncated_upstream_tail_is_quarantined() {
        let (prewarm_url, _prewarm_task) = start_hls_large_mp4_upstream().await;
        let (truncated_url, _truncated_task) = start_hls_truncated_tail_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let backup_url = "https://backup.example/video.m4s".to_owned();
        let mut prewarm_session =
            hls_session_with_backups("session-1", &prewarm_url, vec![backup_url.clone()]);
        prewarm_session.variant.video.request.size = Some(large_fake_mp4().len() as u64);
        prewarm_session.variant.video.request.bandwidth = Some(0);
        state
            .hls_cache
            .prewarm_session_first_frame_with_control(
                &state.hls_upstream_client,
                &prewarm_session,
                || crate::hls_cache::HlsCacheFillControl::Continue,
            )
            .await
            .expect("session should prewarm");
        let mut playback_session =
            hls_session_with_backups("session-1", &truncated_url, vec![backup_url.clone()]);
        playback_session.variant.video.request.size = Some(large_fake_mp4().len() as u64);
        playback_session.variant.video.request.bandwidth = Some(0);
        let request = playback_session.variant.video.request.clone();
        state.cdn_history.record_request(
            &request,
            &truncated_url,
            CdnObservation::playback_complete(
                4096,
                Duration::from_millis(10),
                Duration::from_millis(1),
                Some(true),
            ),
        );
        assert_eq!(truncated_url, state.cdn_history.rank_request(&request)[0]);
        insert_authorized_hls_session(&state, playback_session);

        let prewarmed = state
            .hls_cache
            .prewarmed_resource("session-1", "video.m4s")
            .expect("prewarm metadata should load");
        let mut headers = HeaderMap::new();
        headers.insert(
            RANGE,
            HeaderValue::from_str(&format!(
                "bytes={}-{}",
                prewarmed.initialization_length,
                prewarmed.total_length - 1
            ))
            .expect("range header should be valid"),
        );
        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("short but cleanly terminated tail should reach the client");
        assert!(
            body.len()
                < usize::try_from(prewarmed.total_length - prewarmed.initialization_length)
                    .unwrap()
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.cdn_history.rank_request(&request)[0] != backup_url {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("truncated tail should quarantine its CDN representation");
    }

    #[tokio::test]
    async fn hls_segment_serves_prewarmed_prefix_range() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session("session-1", &upstream_url);
        state
            .hls_cache
            .prewarm_session_first_frame_with_control(&state.hls_upstream_client, &session, || {
                crate::hls_cache::HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm");
        insert_authorized_hls_session(
            &state,
            hls_session("session-1", "http://127.0.0.1:9/unreachable.m4s"),
        );
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!(
            format!("bytes 1-3/{}", fake_mp4().len()),
            response.headers()[CONTENT_RANGE]
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&fake_mp4()[1..=3], &body[..]);
    }

    #[tokio::test]
    async fn hls_segment_streams_large_full_span_in_bounded_chunks() {
        let (upstream_url, _upstream_task) = start_hls_large_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = Some(large_fake_mp4().len() as u64);
        insert_authorized_hls_session(&state, session);

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        assert_eq!(
            Some(large_fake_mp4().len() as u64),
            response_content_length(&response)
        );
        let mut stream = response.into_body().into_data_stream();
        let mut received = Vec::new();
        let mut chunk_count = 0;
        while let Some(chunk) = stream
            .try_next()
            .await
            .expect("foreground range stream should remain valid")
        {
            assert!(chunk.len() <= HLS_MEDIA_STREAM_CHUNK_BYTES as usize);
            chunk_count += 1;
            received.extend_from_slice(&chunk);
        }

        assert!(
            chunk_count > 1,
            "full media span should not be one Bytes body"
        );
        assert_eq!(large_fake_mp4(), received);
    }

    #[tokio::test]
    async fn foreground_read_joins_inflight_background_range() {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (upstream_url, _upstream_task) =
            start_hls_counted_range_upstream(Arc::clone(&requests)).await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let data = large_fake_mp4();
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = Some(data.len() as u64);
        let resource = session.variant.video.clone();
        insert_authorized_hls_session(&state, session);

        let cache = state.hls_cache.clone();
        let client = state.hls_upstream_client.clone();
        let background_resource = resource.clone();
        let background = tokio::spawn(async move {
            cache
                .fill_missing_resource_ranges(
                    &client,
                    "session-1",
                    &background_resource,
                    HlsRangePriority::Background,
                    &|| HlsCacheFillControl::Continue,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !requests
                    .lock()
                    .expect("request log should be readable")
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background fill should start its first canonical range");

        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=0-10"));
        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;
        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        let body = to_bytes(response.into_body(), 11)
            .await
            .expect("foreground range body should be bounded");
        assert_eq!(&data[..11], &body[..]);

        let background_status = tokio::time::timeout(Duration::from_secs(3), background)
            .await
            .expect("background fill should stop at the foreground priority boundary")
            .expect("background worker should not panic");
        assert!(matches!(background_status, Err(HlsRangeError::Preempted)));
        assert!(
            state
                .hls_cache
                .range_durable_bytes("session-1", &resource)
                .expect("foreground range should be durable")
                > 0
        );
        let first_chunk_requests = requests
            .lock()
            .expect("request log should be readable")
            .iter()
            .filter(|range| range.as_str() == "bytes=0-524287")
            .count();
        assert_eq!(
            1, first_chunk_requests,
            "foreground must join the fill flight"
        );
    }

    #[tokio::test]
    async fn completed_runtime_publication_does_not_cancel_matching_range_control() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", "https://example.test/video.m4s");
        session.variant.video.request.size = None;
        let generation = insert_authorized_hls_session(&state, session.clone());
        let control = hls_range_control(
            Arc::new(state.clone()),
            session.id.clone(),
            generation,
            session.variant.video.clone(),
        );
        let mut completed = session;
        completed.variant.video.request.size = Some(fake_mp4().len() as u64);

        state.register_completed_hls_runtime_session(&completed);

        assert_eq!(HlsCacheFillControl::Continue, control());
    }

    #[tokio::test]
    async fn completed_runtime_publication_keeps_inflight_foreground_range() {
        let started = Arc::new(Notify::new());
        let (release, release_rx) = watch::channel(false);
        let (upstream_url, _upstream_task) =
            start_hls_gated_mp4_upstream(Arc::clone(&started), release_rx).await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = None;
        insert_authorized_hls_session(&state, session.clone());
        let request_state = State(MediaState::new(state.clone()));
        let request = tokio::spawn(async move {
            let mut headers = HeaderMap::new();
            headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));
            hls_segment_get(
                request_state,
                Path(("session-1".to_owned(), "video.m4s".to_owned())),
                headers,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("foreground range should reach the gated upstream");
        let mut completed = session;
        completed.variant.video.request.size = Some(fake_mp4().len() as u64);
        state.register_completed_hls_runtime_session(&completed);
        release.send_replace(true);

        let response = tokio::time::timeout(Duration::from_secs(3), request)
            .await
            .expect("foreground request should finish after releasing the upstream")
            .expect("foreground request should not panic");
        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!(
            format!("bytes 1-3/{}", fake_mp4().len()),
            response.headers()[CONTENT_RANGE]
        );
        let body = to_bytes(response.into_body(), 3)
            .await
            .expect("foreground response body should fit the requested range");
        assert_eq!(&fake_mp4()[1..=3], &body[..]);
    }

    #[tokio::test]
    async fn completed_runtime_publication_cancels_different_content_with_same_session_id() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session("session-1", "https://example.test/video.m4s");
        let generation = insert_authorized_hls_session(&state, session.clone());
        let control = hls_range_control(
            Arc::new(state.clone()),
            session.id.clone(),
            generation,
            session.variant.video.clone(),
        );
        let mut replacement = session;
        replacement.variant.video.request.cache_key.content_id = "different-content".to_owned();
        replacement.variant.video.request.cache_key.source_hash = "different-source".to_owned();

        state.register_completed_hls_runtime_session(&replacement);

        assert_eq!(HlsCacheFillControl::Cancel, control());
    }

    #[tokio::test]
    async fn ordinary_runtime_reinsertion_does_not_continue_old_range_control() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session("session-1", "https://example.test/video.m4s");
        let generation = insert_authorized_hls_session(&state, session.clone());
        let control = hls_range_control(
            Arc::new(state.clone()),
            session.id.clone(),
            generation,
            session.variant.video.clone(),
        );

        state.register_hls_playback_session(session);

        assert_eq!(HlsCacheFillControl::Cancel, control());
    }

    #[tokio::test]
    async fn removed_session_cancels_old_range_control() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session("session-1", "https://example.test/video.m4s");
        let generation = insert_authorized_hls_session(&state, session.clone());
        let control = hls_range_control(
            Arc::new(state.clone()),
            session.id.clone(),
            generation,
            session.variant.video.clone(),
        );

        state.remove_hls_playback_session(&session.id);

        assert_eq!(HlsCacheFillControl::Cancel, control());
    }

    #[tokio::test]
    async fn hls_segment_serves_small_seek_range_from_coordinated_cache() {
        let (upstream_url, _upstream_task) = start_hls_large_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let data = large_fake_mp4();
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = Some(data.len() as u64);
        insert_authorized_hls_session(&state, session);
        let range_start = 700_000_usize;
        let range_end = range_start + 65_535;
        let mut headers = HeaderMap::new();
        headers.insert(
            RANGE,
            HeaderValue::from_str(&format!("bytes={range_start}-{range_end}"))
                .expect("seek range header should be valid"),
        );

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!(
            format!("bytes {range_start}-{range_end}/{}", data.len()),
            response.headers()[CONTENT_RANGE]
        );
        assert_eq!(Some(65_536), response_content_length(&response));
        let body = to_bytes(response.into_body(), 65_536)
            .await
            .expect("seek range body should fit its declared bound");
        assert_eq!(&data[range_start..=range_end], &body[..]);
    }

    #[tokio::test]
    async fn hls_segment_uses_non_caching_proxy_when_cache_quota_is_exhausted() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            hls_cache_max_bytes: 1,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session("session-1", &upstream_url);
        let resource = session.variant.video.clone();
        insert_authorized_hls_session(&state, session);
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!(
            format!("bytes 1-3/{}", fake_mp4().len()),
            response.headers()[CONTENT_RANGE]
        );
        let body = to_bytes(response.into_body(), 4)
            .await
            .expect("fallback range body should match its bound");
        assert_eq!(&fake_mp4()[1..=3], &body[..]);
        assert_eq!(
            0,
            state
                .hls_cache
                .range_durable_bytes("session-1", &resource)
                .expect("quota rejection should leave no durable range")
        );
    }

    #[tokio::test]
    async fn quota_fallback_retries_ignored_range_as_non_caching_full_get() {
        let (upstream_url, _upstream_task) = start_hls_range_ignored_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            hls_cache_max_bytes: 1,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = None;
        let resource = session.variant.video.clone();
        insert_authorized_hls_session(&state, session);
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        assert_eq!(
            Some(HLS_INITIALIZATION_SCAN_BYTES + 1),
            response_content_length(&response)
        );
        let body = to_bytes(
            response.into_body(),
            usize::try_from(HLS_INITIALIZATION_SCAN_BYTES + 1).unwrap(),
        )
        .await
        .expect("quota fallback should stream the validated full response");
        assert_eq!(
            vec![0x5a; usize::try_from(HLS_INITIALIZATION_SCAN_BYTES + 1).unwrap()],
            body.to_vec()
        );
        assert_eq!(
            0,
            state
                .hls_cache
                .range_durable_bytes("session-1", &resource)
                .expect("quota fallback should not create durable range data")
        );
    }

    #[tokio::test]
    async fn hls_segment_head_returns_metadata_without_range_fill() {
        let (upstream_url, _upstream_task) = start_hls_large_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = Some(large_fake_mp4().len() as u64);
        let resource = session.variant.video.clone();
        insert_authorized_hls_session(&state, session);

        let response = hls_segment_head(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        assert_eq!(
            Some(large_fake_mp4().len() as u64),
            response_content_length(&response)
        );
        assert_eq!(
            0,
            state
                .hls_cache
                .range_durable_bytes("session-1", &resource)
                .expect("HEAD should not create range data")
        );
        let body = to_bytes(response.into_body(), 0)
            .await
            .expect("HEAD body should be empty");
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn hls_segment_rejects_multiple_ranges_before_fetch() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let session = hls_session("session-1", "http://127.0.0.1:9/unreachable.m4s");
        insert_authorized_hls_session(&state, session.clone());
        let mut headers = HeaderMap::new();
        headers.append(RANGE, HeaderValue::from_static("bytes=1-2"));
        headers.append(RANGE, HeaderValue::from_static("bytes=4-5"));

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::RANGE_NOT_SATISFIABLE, response.status());
        assert_eq!(
            format!("bytes */{}", fake_mp4().len()),
            response.headers()[CONTENT_RANGE]
        );
        assert_eq!(
            0,
            state
                .hls_cache
                .range_durable_bytes("session-1", &session.variant.video)
                .expect("invalid range should not create durable data")
        );
    }

    #[tokio::test]
    async fn hls_segment_serves_cached_resource_with_range() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let cached_session = hls_session("session-1", &upstream_url);
        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &cached_session)
            .await
            .expect("session should cache");
        insert_authorized_hls_session(
            &state,
            hls_session("session-1", "http://127.0.0.1:9/unreachable.m4s"),
        );
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!(
            format!("bytes 1-3/{}", fake_mp4().len()),
            response.headers()[CONTENT_RANGE]
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&fake_mp4()[1..=3], &body[..]);
    }

    #[tokio::test]
    async fn hls_segment_serves_hidden_completed_source_from_cache_only() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let cached_session = hls_session("session-1", &upstream_url);
        state
            .hls_cache
            .cache_session_resources(&state.hls_upstream_client, &cached_session)
            .await
            .expect("source session should cache");
        assert!(
            state
                .hls_cache
                .open_cached_resource("session-1", "video.m4s")
                .is_some()
        );

        let mut completed_session = hls_session("session-1", "http://127.0.0.1:9/generated.m4s");
        completed_session.variant.id = "transcoded-h264".to_owned();
        completed_session.variant.video.id = "transcoded.m4s".to_owned();
        completed_session.variant.video.request.url.clear();
        completed_session.variant.video.request.backup_urls.clear();
        completed_session.variant.video.request.headers.clear();
        completed_session
            .variant
            .video
            .request
            .cache_key
            .source_hash = "transcoded-video-source".to_owned();
        completed_session.alternate_variants = vec![cached_session.variant.clone()];
        completed_session.advertise_alternate_variants = false;
        insert_authorized_hls_session(&state, completed_session);

        let playlist_response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(StatusCode::OK, playlist_response.status());
        let playlist_body = to_bytes(playlist_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let playlist_body = String::from_utf8(playlist_body.to_vec()).unwrap();
        assert!(playlist_body.contains("video.m4s"));

        let segment_response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(StatusCode::OK, segment_response.status());
        let body = to_bytes(segment_response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&fake_mp4()[..], &body[..]);
    }

    #[tokio::test]
    async fn hls_segment_rejects_hidden_completed_source_without_cache() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut source_session = hls_session("session-1", "http://127.0.0.1:9/source.m4s");
        source_session.variant.video.request.url.clear();
        source_session.variant.video.request.backup_urls.clear();
        source_session.variant.video.request.headers.clear();
        let mut completed_session = hls_session("session-1", "http://127.0.0.1:9/generated.m4s");
        completed_session.variant.id = "transcoded-h264".to_owned();
        completed_session.variant.video.id = "transcoded.m4s".to_owned();
        completed_session.variant.video.request.url.clear();
        completed_session.variant.video.request.backup_urls.clear();
        completed_session.variant.video.request.headers.clear();
        completed_session
            .variant
            .video
            .request
            .cache_key
            .source_hash = "transcoded-video-source".to_owned();
        completed_session.alternate_variants = vec![source_session.variant.clone()];
        completed_session.advertise_alternate_variants = false;
        insert_authorized_hls_session(&state, completed_session);

        let playlist_response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(StatusCode::NOT_FOUND, playlist_response.status());

        let segment_response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(StatusCode::NOT_FOUND, segment_response.status());
    }

    #[tokio::test]
    async fn hls_hidden_runtime_variant_serves_stale_completion_transition_requests() {
        let (upstream_url, _upstream_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session_with_alternate("session-1", &upstream_url);
        assert_eq!(
            Some(fake_mp4().len() as u64),
            session.alternate_variants[0].video.request.size
        );
        let mut audio_request = media_request(&upstream_url, Vec::new());
        audio_request.kind = BilibiliMediaRequestKind::Audio;
        audio_request.mime_type = Some("audio/mp4".to_owned());
        assert_eq!(Some(fake_mp4().len() as u64), audio_request.size);
        session.alternate_variants[0]
            .codecs
            .push("mp4a.40.2".to_owned());
        session.alternate_variants[0].audio = Some(HlsMediaResource {
            id: "v1-audio.m4s".to_owned(),
            request: audio_request,
        });
        session.advertise_alternate_variants = false;
        assert!(!session.master_playlist().contains("segments/v1-video.m3u8"));
        assert!(!session.master_playlist().contains("segments/v1-audio.m3u8"));
        insert_authorized_hls_session(&state, session);

        for (playlist_id, segment_id) in [
            ("v1-video.m3u8", "v1-video.m4s"),
            ("v1-audio.m3u8", "v1-audio.m4s"),
        ] {
            let playlist_response = hls_segment_get(
                State(MediaState::new(state.clone())),
                Path(("session-1".to_owned(), playlist_id.to_owned())),
                HeaderMap::new(),
            )
            .await;
            assert_eq!(StatusCode::OK, playlist_response.status());
            let playlist_body = to_bytes(playlist_response.into_body(), usize::MAX)
                .await
                .unwrap();
            let playlist_body = String::from_utf8(playlist_body.to_vec()).unwrap();
            assert!(playlist_body.contains(segment_id));

            let mut headers = HeaderMap::new();
            headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));
            let segment_response = hls_segment_get(
                State(MediaState::new(state.clone())),
                Path(("session-1".to_owned(), segment_id.to_owned())),
                headers,
            )
            .await;
            assert_eq!(StatusCode::PARTIAL_CONTENT, segment_response.status());
            assert_eq!(
                format!("bytes 1-3/{}", fake_mp4().len()),
                segment_response.headers()[CONTENT_RANGE]
            );
            let body = to_bytes(segment_response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(&fake_mp4()[1..=3], &body[..]);
        }
    }

    #[tokio::test]
    async fn hls_segment_retries_backup_url_after_retryable_status() {
        let (primary_url, _primary_task) = start_hls_forbidden_upstream().await;
        let (backup_url, _backup_task) = start_hls_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session_with_backups("session-1", &primary_url, vec![backup_url]);
        session.variant.video.request.size = Some(b"video-data".len() as u64);
        insert_authorized_hls_session(&state, session);

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(b"video-data", &body[..]);
    }

    #[tokio::test]
    async fn hls_segment_slow_body_uses_header_latency_for_policy() {
        let (upstream_url, _upstream_task) = start_hls_slow_body_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = Some(b"video-data".len() as u64);
        insert_authorized_hls_session(&state, session);

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(b"video-data", &body[..]);
        let snapshot = state.hls_network_policy.snapshot();
        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::Normal,
            snapshot.state
        );
        assert_eq!(0, snapshot.retrying_variant_count);
        assert_eq!(0, snapshot.degraded_session_count);
    }

    #[tokio::test]
    async fn hls_head_with_conflicting_length_does_not_clear_cdn_quarantine() {
        let (wrong_url, _wrong_task) = start_hls_conflicting_head_upstream().await;
        let (correct_url, _correct_task) = start_hls_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let backup_url = correct_url.clone();
        let wrong_request = media_request(&wrong_url, vec![backup_url.clone()]);
        state.cdn_history.record_request(
            &wrong_request,
            &wrong_url,
            CdnObservation::failure(
                CdnObservationSource::Playback,
                CdnObservationOutcome::IntegrityMismatch,
            ),
        );
        let wrong_session =
            hls_session_with_backups("session-1", &wrong_url, vec![backup_url.clone()]);
        let wrong_resource = wrong_session.variant.video.clone();
        let wrong_generation = insert_authorized_hls_session(&state, wrong_session);
        let wrong_media_state = MediaState::new(state.clone());
        let wrong_policy_recorder = HlsNetworkPolicyRecorder::new(
            &wrong_media_state,
            "session-1".to_owned(),
            wrong_generation,
            WeakNetworkPreference::Adaptive,
        );
        let before_wrong_head = state.cdn_history.rank_request(&wrong_request);
        assert_eq!(backup_url, before_wrong_head[0]);

        let wrong_response = send_hls_upstream_request(
            HlsMediaProxyContext {
                state: &wrong_media_state,
                variant_id: "h264",
                resource: &wrong_resource,
                policy_recorder: &wrong_policy_recorder,
            },
            &wrong_url,
            &HeaderMap::new(),
            true,
        )
        .await
        .expect("wrong HEAD response should arrive")
        .response;

        assert_eq!(StatusCode::OK, wrong_response.status());
        assert_eq!(
            Some(&HeaderValue::from_static("999")),
            wrong_response.headers().get(CONTENT_LENGTH)
        );
        assert!(
            to_bytes(wrong_response.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            before_wrong_head,
            state.cdn_history.rank_request(&wrong_request),
            "conflicting HEAD metadata must not clear representation quarantine"
        );
        let correct_state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("correct-tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let correct_request = media_request(
            &correct_url,
            vec!["http://127.0.0.1:9/backup.m4s".to_owned()],
        );
        correct_state.cdn_history.record_request(
            &correct_request,
            &correct_url,
            CdnObservation::failure(
                CdnObservationSource::Playback,
                CdnObservationOutcome::IntegrityMismatch,
            ),
        );
        assert_eq!(
            "http://127.0.0.1:9/backup.m4s",
            correct_state.cdn_history.rank_request(&correct_request)[0],
            "the quarantined representation should rank behind the unknown control"
        );
        let correct_session = hls_session_with_backups(
            "session-1",
            &correct_url,
            vec!["http://127.0.0.1:9/backup.m4s".to_owned()],
        );
        let correct_resource = correct_session.variant.video.clone();
        let correct_generation = insert_authorized_hls_session(&correct_state, correct_session);
        let correct_media_state = MediaState::new(correct_state.clone());
        let correct_policy_recorder = HlsNetworkPolicyRecorder::new(
            &correct_media_state,
            "session-1".to_owned(),
            correct_generation,
            WeakNetworkPreference::Adaptive,
        );
        let correct_response = send_hls_upstream_request(
            HlsMediaProxyContext {
                state: &correct_media_state,
                variant_id: "h264",
                resource: &correct_resource,
                policy_recorder: &correct_policy_recorder,
            },
            &correct_url,
            &HeaderMap::new(),
            true,
        )
        .await
        .expect("consistent HEAD response should arrive")
        .response;

        assert_eq!(StatusCode::OK, correct_response.status());
        assert_eq!(
            Some(fake_mp4().len() as u64),
            correct_response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
        );
        assert_eq!(
            correct_url,
            correct_state.cdn_history.rank_request(&correct_request)[0],
            "a consistent HEAD should clear quarantine as the control"
        );
    }

    #[tokio::test]
    async fn hls_segment_retries_backup_url_after_ignored_range() {
        let (primary_url, _primary_task) = start_hls_range_ignored_upstream().await;
        let (backup_url, _backup_task) = start_hls_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session_with_backups("session-1", &primary_url, vec![backup_url]);
        session.variant.video.request.size = Some(b"video-data".len() as u64);
        insert_authorized_hls_session(&state, session);
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        assert_eq!("bytes 1-3/10", response.headers()[CONTENT_RANGE]);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(b"ide", &body[..]);
    }

    #[tokio::test]
    async fn hls_segment_falls_back_to_validated_full_get_when_range_is_unsupported() {
        let (upstream_url, _upstream_task) = start_hls_range_ignored_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        let mut session = hls_session("session-1", &upstream_url);
        session.variant.video.request.size = None;
        let resource = session.variant.video.clone();
        insert_authorized_hls_session(&state, session);
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::OK, response.status());
        assert_eq!(
            Some(HLS_INITIALIZATION_SCAN_BYTES + 1),
            response_content_length(&response)
        );
        let body = to_bytes(
            response.into_body(),
            usize::try_from(HLS_INITIALIZATION_SCAN_BYTES + 1).unwrap(),
        )
        .await
        .expect("unsupported-range fallback should stream the validated full body");
        assert_eq!(
            vec![0x5a; usize::try_from(HLS_INITIALIZATION_SCAN_BYTES + 1).unwrap()],
            body.to_vec()
        );
        assert_eq!(
            0,
            state
                .hls_cache
                .range_durable_bytes("session-1", &resource)
                .expect("non-caching fallback should not write range data")
        );
    }

    #[tokio::test]
    async fn hls_segment_rejects_upstream_that_shifts_segment_range() {
        let (upstream_url, _upstream_task) = start_hls_shifted_range_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        insert_authorized_hls_session(&state, hls_session("session-1", &upstream_url));
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state.clone())),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::BAD_GATEWAY, response.status());
        assert_eq!(
            crate::hls_network_policy::HlsWeakNetworkState::UpstreamFailed,
            state.hls_network_policy.snapshot().state
        );
    }

    #[tokio::test]
    async fn hls_media_playlist_rejects_upstream_that_ignores_initialization_range() {
        let (upstream_url, _upstream_task) = start_hls_range_ignored_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        insert_authorized_hls_session(&state, hls_session("session-1", &upstream_url));

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::BAD_GATEWAY, response.status());
    }

    #[tokio::test]
    async fn hls_media_playlist_rejects_oversized_chunked_initialization_probe() {
        let (upstream_url, _upstream_task) = start_hls_oversized_chunked_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let root_path = temp.path().canonicalize().unwrap();
        let state = AppState::new(CacheServerOptions {
            root_path: root_path.clone(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });
        insert_authorized_hls_session(&state, hls_session("session-1", &upstream_url));

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m3u8".to_owned())),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(StatusCode::BAD_GATEWAY, response.status());
    }

    #[test]
    fn invalid_hls_fallback_content_type_uses_octet_stream() {
        let source = HeaderMap::new();
        let mut target = HeaderMap::new();

        copy_hls_upstream_headers(&source, &mut target, "video/mp4\nx-invalid: nope");

        assert_eq!("application/octet-stream", target[CONTENT_TYPE]);
    }

    #[tokio::test]
    async fn cached_file_response_invalid_content_type_uses_octet_stream() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("video.m4s");
        std::fs::write(&path, b"media").expect("media file should be written");
        let opened_file = OpenedMediaFile {
            file: std::fs::File::open(&path).expect("media file should open"),
            content_type: "video/mp4\nx-invalid: nope".to_owned(),
            last_modified: std::time::SystemTime::UNIX_EPOCH,
            size_bytes: 5,
        };

        let response = build_file_response(opened_file, None, true).await;

        assert_eq!("application/octet-stream", response.headers()[CONTENT_TYPE]);
    }

    #[test]
    fn parses_mp4_initialization_length_through_moov_box() {
        let mp4 = fake_mp4();

        assert_eq!(Some(28), mp4_initialization_length(&mp4));
    }

    fn parse(value: &'static str, size: u64) -> Option<ByteRange> {
        parse_range(Some(&HeaderValue::from_static(value)), size).unwrap()
    }

    fn insert_authorized_hls_session(state: &AppState, session: HlsPlaybackSession) -> u64 {
        let session_id = session.id.clone();
        let variant_id = session.variant.id.clone();
        let playback_source = PlaybackSource {
            item_id: session_id.clone(),
            variant_id: variant_id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{session_id}/master.m3u8"),
            expires_at: None,
        };
        let playback_session = BilibiliPlaybackSession {
            id: session_id.clone(),
            title: session.title.clone(),
            content_id: "cid-1".to_owned(),
            selected_variant_id: variant_id.clone(),
            selected_variant: Some(BilibiliPlaybackVariant {
                id: variant_id,
                label: "1920x1080".to_owned(),
                source_kind: "dash".to_owned(),
                container: "mp4".to_owned(),
                video_codec: "avc1.640028".to_owned(),
                audio_codec: String::new(),
                width: session
                    .variant
                    .width
                    .and_then(|width| i32::try_from(width).ok())
                    .unwrap_or_default(),
                height: session
                    .variant
                    .height
                    .and_then(|height| i32::try_from(height).ok())
                    .unwrap_or_default(),
                bitrate: i64::try_from(session.variant.bandwidth).unwrap_or_default(),
                size_bytes: 0,
            }),
            variants: Vec::new(),
            transcoding_plan: None,
            effective_policy: Some(session.effective_policy.to_proto()),
        };
        let generation = state.register_hls_playback_session(session);
        let task = state
            .tasks
            .create_bilibili_playback_task(&format!("BV1{session_id}"), None, None)
            .expect("playback task should be created");
        state
            .tasks
            .complete_playback_results_playable(
                &task.task.id,
                "Playable playback".to_owned(),
                "Result is playable.".to_owned(),
                playback_source.clone(),
                playback_session.clone(),
                vec![BilibiliTaskResultItem {
                    id: session_id.clone(),
                    selection_id: "page:1".to_owned(),
                    title: "Episode".to_owned(),
                    subtitle: String::new(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-1".to_owned(),
                    index: 1,
                    state: TaskState::Playable.into(),
                    message: "Playable".to_owned(),
                    library_item_id: String::new(),
                    playback_source: Some(playback_source),
                    playback_session: Some(playback_session),
                    identity: None,
                    hls_cache_fill_status: None,
                }],
            )
            .expect("playback task should authorize HLS session");
        assert!(
            state
                .tasks
                .is_playback_result_session_playable(&session_id, false),
            "inserted HLS fixture session should be authorized through its result item"
        );
        generation
    }

    async fn start_hls_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/video.m4s", get(upstream_get).head(upstream_head)),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_mp4_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/video.m4s", get(upstream_mp4_get).head(upstream_mp4_head)),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_gated_mp4_upstream(
        started: Arc<Notify>,
        release: watch::Receiver<bool>,
    ) -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get(move |headers: HeaderMap| {
                        let started = Arc::clone(&started);
                        let mut release = release.clone();
                        async move {
                            started.notify_one();
                            if !*release.borrow() {
                                let _ = release.wait_for(|released| *released).await;
                            }
                            upstream_mp4_response(headers, false)
                        }
                    }),
                ),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_slow_body_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get(upstream_slow_body_get).head(upstream_head),
                ),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_slow_initialization_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get(upstream_slow_initialization_get).head(upstream_mp4_head),
                ),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_large_mp4_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get(upstream_large_mp4_get).head(upstream_large_mp4_head),
                ),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_counted_range_upstream(
        requests: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/video.m4s", get(upstream_counted_range_get))
                    .with_state(requests),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_truncated_tail_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/video.m4s", get(upstream_truncated_tail_get)),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_conflicting_head_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/video.m4s", axum::routing::head(upstream_conflicting_head)),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_range_ignored_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get(upstream_range_ignored).head(upstream_range_ignored),
                ),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_shifted_range_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get(upstream_shifted_range).head(upstream_shifted_range),
                ),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_oversized_chunked_mp4_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get(upstream_oversized_chunked_mp4).head(upstream_oversized_chunked_mp4),
                ),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn start_hls_forbidden_upstream() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/video.m4s",
                    get(upstream_forbidden).head(upstream_forbidden),
                ),
            )
            .await
            .expect("upstream server should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn upstream_get(headers: HeaderMap) -> Response<Body> {
        upstream_media_response(headers, false)
    }

    async fn upstream_head(headers: HeaderMap) -> Response<Body> {
        upstream_media_response(headers, true)
    }

    async fn upstream_mp4_get(headers: HeaderMap) -> Response<Body> {
        upstream_mp4_response(headers, false)
    }

    async fn upstream_mp4_head(headers: HeaderMap) -> Response<Body> {
        upstream_mp4_response(headers, true)
    }

    async fn upstream_slow_body_get(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return empty_response(StatusCode::FORBIDDEN);
        }

        let stream = futures_util::stream::unfold(0_u8, |index| async move {
            match index {
                0 => Some((Ok::<Bytes, Infallible>(Bytes::from_static(b"video-")), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(3_100)).await;
                    Some((Ok::<Bytes, Infallible>(Bytes::from_static(b"data")), 2))
                }
                _ => None,
            }
        });
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .body(Body::from_stream(stream))
            .expect("slow-body upstream response should build")
    }

    async fn upstream_slow_initialization_get(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return empty_response(StatusCode::FORBIDDEN);
        }

        let data = fake_mp4();
        let Some((start, end)) = headers
            .get(RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|range| parse_test_range(range, data.len()))
        else {
            return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
        };
        let chunk = Bytes::from(data[start..=end].to_vec());
        let total_len = data.len();
        let stream = futures_util::stream::unfold(Some(chunk), |chunk| async move {
            let chunk = chunk?;
            tokio::time::sleep(Duration::from_millis(3_100)).await;
            Some((Ok::<Bytes, Infallible>(chunk), None))
        });
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{total_len}"))
            .body(Body::from_stream(stream))
            .expect("slow initialization upstream response should build")
    }

    async fn upstream_large_mp4_get(headers: HeaderMap) -> Response<Body> {
        upstream_large_mp4_response(headers, false)
    }

    async fn upstream_large_mp4_head(headers: HeaderMap) -> Response<Body> {
        upstream_large_mp4_response(headers, true)
    }

    async fn upstream_counted_range_get(
        State(requests): State<Arc<std::sync::Mutex<Vec<String>>>>,
        headers: HeaderMap,
    ) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return empty_response(StatusCode::FORBIDDEN);
        }
        let Some(range_header) = headers.get(RANGE).and_then(|value| value.to_str().ok()) else {
            return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
        };
        let data = large_fake_mp4();
        let Some((start, end)) = parse_test_range(range_header, data.len()) else {
            return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
        };
        requests
            .lock()
            .expect("request log should be writable")
            .push(range_header.to_owned());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let body = data[start..=end].to_vec();
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{}", data.len()))
            .header(CONTENT_LENGTH, body.len().to_string())
            .body(Body::from(body))
            .expect("counted range response should build")
    }

    async fn upstream_truncated_tail_get(headers: HeaderMap) -> Response<Body> {
        let data = large_fake_mp4();
        let Some(range_header) = headers.get(RANGE).and_then(|value| value.to_str().ok()) else {
            return upstream_large_mp4_response(headers, false);
        };
        let Some((start, end)) = parse_test_range(range_header, data.len()) else {
            return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
        };
        if start == 0 {
            return upstream_large_mp4_response(headers, false);
        }

        let truncated = Bytes::from(data[start..=(start + 63).min(end)].to_vec());
        let body = Body::from_stream(futures_util::stream::iter([Ok::<_, Infallible>(truncated)]));
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{}", data.len()))
            .body(body)
            .expect("truncated range response should build")
    }

    async fn upstream_conflicting_head(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return empty_response(StatusCode::FORBIDDEN);
        }
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, "999")
            .body(Body::empty())
            .expect("conflicting HEAD response should build")
    }

    async fn upstream_forbidden() -> Response<Body> {
        empty_response(StatusCode::FORBIDDEN)
    }

    async fn upstream_range_ignored() -> Response<Body> {
        let body = vec![0x5a; usize::try_from(HLS_INITIALIZATION_SCAN_BYTES + 1).unwrap()];
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, body.len().to_string())
            .body(Body::from(body))
            .expect("range-ignored upstream response should build")
    }

    async fn upstream_shifted_range() -> Response<Body> {
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_RANGE, "bytes 0-2/10")
            .body(Body::from("vid"))
            .expect("shifted-range upstream response should build")
    }

    async fn upstream_oversized_chunked_mp4() -> Response<Body> {
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(
                CONTENT_RANGE,
                format!(
                    "bytes 0-{}/{}",
                    HLS_INITIALIZATION_SCAN_BYTES,
                    2 * 1024 * 1024
                ),
            )
            .body(Body::from(vec![
                0_u8;
                usize::try_from(
                    HLS_INITIALIZATION_SCAN_BYTES + 1
                )
                .unwrap()
            ]))
            .expect("oversized chunked upstream response should build")
    }

    fn upstream_media_response(headers: HeaderMap, head_only: bool) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return empty_response(StatusCode::FORBIDDEN);
        }

        let data = b"video-data";
        if headers.get(RANGE) == Some(&HeaderValue::from_static("bytes=1-3")) {
            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_TYPE, "video/mp4")
                .header(CONTENT_RANGE, "bytes 1-3/10")
                .body(if head_only {
                    Body::empty()
                } else {
                    Body::from(data[1..=3].to_vec())
                })
                .expect("partial upstream response should build");
        }

        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, data.len().to_string())
            .body(if head_only {
                Body::empty()
            } else {
                Body::from(data.to_vec())
            })
            .expect("upstream response should build")
    }

    fn upstream_mp4_response(headers: HeaderMap, head_only: bool) -> Response<Body> {
        upstream_mp4_bytes_response(headers, head_only, fake_mp4())
    }

    fn upstream_large_mp4_response(headers: HeaderMap, head_only: bool) -> Response<Body> {
        upstream_mp4_bytes_response(headers, head_only, large_fake_mp4())
    }

    fn upstream_mp4_bytes_response(
        headers: HeaderMap,
        head_only: bool,
        data: Vec<u8>,
    ) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return empty_response(StatusCode::FORBIDDEN);
        }

        if let Some(range) = headers.get(RANGE).and_then(|value| value.to_str().ok())
            && let Some((start, end)) = parse_test_range(range, data.len())
        {
            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_TYPE, "video/mp4")
                .header(CONTENT_RANGE, format!("bytes {start}-{end}/{}", data.len()))
                .body(if head_only {
                    Body::empty()
                } else {
                    Body::from(data[start..=end].to_vec())
                })
                .expect("partial upstream MP4 response should build");
        }

        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, data.len().to_string())
            .body(if head_only {
                Body::empty()
            } else {
                Body::from(data)
            })
            .expect("upstream MP4 response should build")
    }

    fn parse_test_range(value: &str, size: usize) -> Option<(usize, usize)> {
        let spec = value.strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        let start = start.parse::<usize>().ok()?;
        let end = end.parse::<usize>().ok()?.min(size.checked_sub(1)?);
        (start <= end).then_some((start, end))
    }

    fn fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes.extend(mp4_box(*b"moof", b"frag"));
        bytes.extend(mp4_box(*b"mdat", b"media-data"));
        bytes
    }

    fn large_fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes.extend(mp4_box(*b"moof", b"frag"));
        bytes.extend(mp4_box(
            *b"mdat",
            &vec![0x55; usize::try_from(HLS_INITIALIZATION_SCAN_BYTES).unwrap() + 64],
        ));
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

    fn hls_session(id: &str, upstream_url: &str) -> HlsPlaybackSession {
        hls_session_with_backups(id, upstream_url, Vec::new())
    }

    fn hls_session_with_alternate(id: &str, upstream_url: &str) -> HlsPlaybackSession {
        let mut session = hls_session_with_backups(id, upstream_url, Vec::new());
        session.variant.id = "h264-1080p".to_owned();
        session.variant.bandwidth = 1_000_000;
        session.alternate_variants = vec![HlsVariant {
            id: "h264-720p".to_owned(),
            bandwidth: 600_000,
            codecs: vec!["avc1.640028".to_owned()],
            width: Some(1280),
            height: Some(720),
            duration_seconds: 60,
            video: HlsMediaResource {
                id: "v1-video.m4s".to_owned(),
                request: media_request(upstream_url, Vec::new()),
            },
            audio: None,
        }];
        session
    }

    fn hls_session_with_backups(
        id: &str,
        upstream_url: &str,
        backup_urls: Vec<String>,
    ) -> HlsPlaybackSession {
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
                    request: media_request(upstream_url, backup_urls),
                },
                audio: None,
            },
            alternate_variants: Vec::new(),
            advertise_alternate_variants: true,
            abr: Default::default(),
            variants: Vec::new(),
            transcoding: Default::default(),
            effective_policy: crate::playback_policy::PlaybackPolicy::default(),
            accepted_identity: None,
        }
    }

    fn media_request(url: &str, backup_urls: Vec<String>) -> BilibiliMediaRequest {
        BilibiliMediaRequest {
            kind: BilibiliMediaRequestKind::Video,
            stream_id: None,
            url: url.to_owned(),
            backup_urls,
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
            size: Some(fake_mp4().len() as u64),
            duration_seconds: Some(60),
            cache_key: BilibiliMediaCacheKey {
                content_id: "content-1".to_owned(),
                media_kind: BilibiliMediaRequestKind::Video,
                stream_id: None,
                codecs: Some("avc1.640028".to_owned()),
                source_hash: "source-hash".to_owned(),
            },
        }
    }
}
