use std::{io::SeekFrom, sync::Arc, time::Duration};

use axum::{
    BoxError,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode,
        header::{
            ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
            IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED, RANGE,
        },
    },
};
use futures_util::{StreamExt, TryStreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    time::Instant,
};
use tokio_util::io::ReaderStream;

use crate::{
    AppState,
    hls::{
        HlsMediaResource, HlsMediaSegment, HlsPlaybackSession, mp4_initialization_length,
        should_forward_media_request_header,
    },
    hls_cache::OpenedPrewarmedHlsResource,
    library::OpenedMediaFile,
    playback_policy::WeakNetworkPreference,
};

const HLS_INITIALIZATION_SCAN_BYTES: u64 = 1024 * 1024;
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");

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
    let tasks = Arc::clone(&state.state.tasks);
    let opened_resource =
        tokio::task::spawn_blocking(move || tasks.open_task_resource(&resource_id)).await;
    let Some(opened_resource) = (match opened_resource {
        Ok(opened_resource) => opened_resource,
        Err(error) => {
            eprintln!("Task resource open worker failed: {error}");
            None
        }
    }) else {
        return resource_not_found_response();
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
    let Some(handle) = state.state.hls_playback_session_for_serving(&session_id) else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let generation = handle.generation;
    let session = handle.session;

    let body = if head_only {
        Body::empty()
    } else {
        let weak_network_preference = session.effective_policy.weak_network_preference;
        Body::from(session.master_playlist_with_variant_filter(|variant| {
            state
                .state
                .hls_network_policy
                .variant_is_advertisable_for_policy(
                    weak_network_preference,
                    &session_id,
                    generation,
                    &variant.id,
                )
        }))
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
    let Some(handle) = state.state.hls_playback_session_for_serving(&session_id) else {
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

    if let Some(opened_file) = state
        .state
        .hls_cache
        .open_cached_resource(&session_id, &resource.id)
    {
        policy_recorder.record_cache_hit();
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
        return build_file_response(opened_file, range, head_only).await;
    }

    if !advertised_resource && !resource_has_upstream(&resource) {
        return empty_response(StatusCode::NOT_FOUND);
    }

    if let Some(opened_file) = state
        .state
        .hls_cache
        .open_prewarmed_resource(&session_id, &resource.id)
        && let Some(range_header) = headers.get(RANGE)
    {
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

    proxy_hls_media_resource(
        &state,
        &policy_recorder,
        &variant_id,
        resource,
        &headers,
        head_only,
    )
    .await
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
    let mut urls = Vec::with_capacity(resource.request.backup_urls.len() + 1);
    urls.push(resource.request.url.clone());
    urls.extend(resource.request.backup_urls.clone());
    let context = HlsMediaProxyContext {
        state,
        variant_id,
        resource: &resource,
        policy_recorder,
    };

    let mut last_retryable_response = None;
    for url in urls {
        match send_hls_upstream_request(context, &url, headers, head_only).await {
            Ok(upstream) if should_retry_hls_upstream_status(upstream.response.status()) => {
                policy_recorder.record_upstream_retry(variant_id);
                last_retryable_response = Some(upstream.response);
            }
            Ok(upstream) => return upstream.response,
            Err(_) => {
                policy_recorder.record_upstream_retry(variant_id);
                continue;
            }
        }
    }

    policy_recorder.record_upstream_failure(variant_id);
    last_retryable_response.unwrap_or_else(|| {
        text_response(
            StatusCode::BAD_GATEWAY,
            "HLS upstream media request failed.\n",
            head_only,
        )
    })
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
    let upstream_headers = upstream.headers().clone();
    if range_response_invalid(headers.get(RANGE), status, &upstream_headers) {
        return Ok(HlsUpstreamResponse {
            response: text_response(
                StatusCode::BAD_GATEWAY,
                "HLS upstream ignored byte range request.\n",
                head_only,
            ),
        });
    }

    let mut response = Response::builder()
        .status(status)
        .body(if head_only {
            if status.is_success() {
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

    Ok(HlsUpstreamResponse { response })
}

struct HlsUpstreamResponse {
    response: Response<Body>,
}

fn hls_policy_recording_body(
    upstream: reqwest::Response,
    policy_recorder: HlsNetworkPolicyRecorder,
    variant_id: String,
    response_time: Duration,
) -> Body {
    Body::from_stream(hls_policy_recording_stream(
        upstream,
        policy_recorder,
        variant_id,
        response_time,
    ))
}

fn hls_policy_recording_stream(
    upstream: reqwest::Response,
    policy_recorder: HlsNetworkPolicyRecorder,
    variant_id: String,
    response_time: Duration,
) -> impl futures_core::Stream<Item = Result<Bytes, reqwest::Error>> {
    let stream = Box::pin(upstream.bytes_stream());
    futures_util::stream::unfold(
        (stream, policy_recorder, variant_id, response_time, false),
        |(mut stream, policy_recorder, variant_id, response_time, failed)| async move {
            match stream.next().await {
                Some(Ok(bytes)) => Some((
                    Ok::<_, reqwest::Error>(bytes),
                    (stream, policy_recorder, variant_id, response_time, failed),
                )),
                Some(Err(error)) => {
                    if !failed {
                        policy_recorder.record_upstream_failure(&variant_id);
                    }
                    Some((
                        Err(error),
                        (stream, policy_recorder, variant_id, response_time, true),
                    ))
                }
                None => {
                    if !failed {
                        policy_recorder.record_upstream_success(&variant_id, response_time);
                    }
                    None
                }
            }
        },
    )
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
                | StatusCode::TOO_MANY_REQUESTS
        )
}

fn range_response_invalid(
    requested_range: Option<&HeaderValue>,
    status: StatusCode,
    headers: &HeaderMap,
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
}

async fn load_hls_mp4_initialization(
    state: &MediaState,
    policy_recorder: &HlsNetworkPolicyRecorder,
    variant_id: &str,
    resource: &HlsMediaResource,
) -> Result<Mp4Initialization, ()> {
    let mut urls = Vec::with_capacity(resource.request.backup_urls.len() + 1);
    urls.push(resource.request.url.clone());
    urls.extend(resource.request.backup_urls.clone());

    for url in urls {
        match load_hls_mp4_initialization_from_url(state, resource, &url).await {
            Ok(probe) => {
                policy_recorder.record_upstream_success(variant_id, probe.response_time);
                return Ok(probe.initialization);
            }
            Err(()) => policy_recorder.record_upstream_retry(variant_id),
        }
    }

    Err(())
}

async fn load_hls_mp4_initialization_from_url(
    state: &MediaState,
    resource: &HlsMediaResource,
    url: &str,
) -> Result<Mp4InitializationProbe, ()> {
    let started_at = Instant::now();
    let upstream = hls_upstream_request_builder(state, resource, Method::GET, url)
        .header(
            RANGE,
            format!("bytes=0-{}", HLS_INITIALIZATION_SCAN_BYTES - 1),
        )
        .send()
        .await
        .map_err(|_| ())?;
    let response_time = started_at.elapsed();
    let status = StatusCode::from_u16(upstream.status().as_u16()).map_err(|_| ())?;
    if should_retry_hls_upstream_status(status) || !status.is_success() {
        return Err(());
    }

    let headers = upstream.headers().clone();
    let content_length = upstream.content_length();
    if status != StatusCode::PARTIAL_CONTENT
        || content_length.is_some_and(|length| length > HLS_INITIALIZATION_SCAN_BYTES)
    {
        return Err(());
    }
    let (returned_range, total_length) = content_range_byte_range(&headers).ok_or(())?;
    if returned_range.start != 0 || returned_range.length() > HLS_INITIALIZATION_SCAN_BYTES {
        return Err(());
    }
    let bytes = read_hls_initialization_probe(upstream).await?;
    let length = mp4_initialization_length(&bytes).ok_or(())?;
    if length == 0 || length >= total_length {
        return Err(());
    }

    Ok(Mp4InitializationProbe {
        initialization: Mp4Initialization {
            length,
            total_length,
            segments: Vec::new(),
        },
        response_time,
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
}

async fn open_hls_upstream_tail_response(
    context: HlsMediaProxyContext<'_>,
    start: u64,
    end: u64,
    total_length: u64,
) -> Result<HlsUpstreamTailResponse, ()> {
    let tail_range = HeaderValue::from_str(&format!("bytes={start}-{end}")).map_err(|_| ())?;
    let mut urls = Vec::with_capacity(context.resource.request.backup_urls.len() + 1);
    urls.push(context.resource.request.url.clone());
    urls.extend(context.resource.request.backup_urls.clone());

    for url in urls {
        let started_at = Instant::now();
        let upstream =
            match hls_upstream_request_builder(context.state, context.resource, Method::GET, &url)
                .header(RANGE.as_str(), tail_range.to_str().map_err(|_| ())?)
                .send()
                .await
            {
                Ok(upstream) => upstream,
                Err(_) => {
                    context
                        .policy_recorder
                        .record_upstream_retry(context.variant_id);
                    continue;
                }
            };
        let response_time = started_at.elapsed();
        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if should_retry_hls_upstream_status(status) || !status.is_success() {
            context
                .policy_recorder
                .record_upstream_retry(context.variant_id);
            continue;
        }
        let headers = upstream.headers();
        let Some((returned_range, returned_total_length)) = content_range_byte_range(headers)
        else {
            context
                .policy_recorder
                .record_upstream_retry(context.variant_id);
            continue;
        };
        let expected_range = ByteRange { start, end };
        if status != StatusCode::PARTIAL_CONTENT
            || returned_range != expected_range
            || returned_total_length != total_length
        {
            context
                .policy_recorder
                .record_upstream_retry(context.variant_id);
            continue;
        }

        return Ok(HlsUpstreamTailResponse {
            upstream,
            response_time,
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
    use tokio::task::JoinHandle;

    use crate::{
        bbdown_adapter::{
            BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest,
            BilibiliMediaRequestKind,
        },
        config::CacheServerOptions,
        generated::tvos_net_player::v1::{
            BilibiliPlaybackSession, BilibiliPlaybackVariant, BilibiliTaskResultItem,
            CacheResourceRef, PlaybackProtocol, PlaybackSource, TaskArtifact, TaskArtifactKind,
            TaskArtifactState, TaskResult, TaskState,
        },
        hls::{HlsMediaResource, HlsPlaybackSession, HlsVariant},
        task_output::TaskResourceRecord,
    };

    struct TaskResourceFixture {
        temp: TempDir,
        state: MediaState,
        resource_id: String,
        resource_path: PathBuf,
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
        fs::create_dir_all(resource_path.parent().unwrap())
            .expect("resource directory should be created");
        if let Some(body) = body {
            fs::write(&resource_path, body).expect("resource body should be written");
        }
        state
            .tasks
            .replace_task_output(
                &task.id,
                vec![TaskResult {
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
                }],
                vec![record],
            )
            .expect("test task output should be replaced");

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
            assert_eq!(
                expected_body,
                &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..]
            );
        }
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
    async fn task_resource_not_found_responses_do_not_disclose_paths() {
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
        assert_path_free_not_found(
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
        assert_path_free_not_found(
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

        assert_path_free_not_found(response, &outside_path).await;
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
        insert_authorized_hls_session(&state, hls_session("session-1", &upstream_url));
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state)),
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
        let session = hls_session("session-1", &upstream_url);
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
            State(MediaState::new(state)),
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
        let session = hls_session("session-1", &upstream_url);
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
        insert_authorized_hls_session(&state, hls_session("session-1", &upstream_url));

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
            State(MediaState::new(state)),
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
        let mut audio_request = media_request(&upstream_url, Vec::new());
        audio_request.kind = BilibiliMediaRequestKind::Audio;
        audio_request.mime_type = Some("audio/mp4".to_owned());
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
        insert_authorized_hls_session(
            &state,
            hls_session_with_backups("session-1", &primary_url, vec![backup_url]),
        );

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
        insert_authorized_hls_session(&state, hls_session("session-1", &upstream_url));

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
        insert_authorized_hls_session(
            &state,
            hls_session_with_backups("session-1", &primary_url, vec![backup_url]),
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
        assert_eq!("bytes 1-3/10", response.headers()[CONTENT_RANGE]);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(b"ide", &body[..]);
    }

    #[tokio::test]
    async fn hls_segment_rejects_upstream_that_ignores_segment_range() {
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
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=1-3"));

        let response = hls_segment_get(
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::BAD_GATEWAY, response.status());
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
            State(MediaState::new(state)),
            Path(("session-1".to_owned(), "video.m4s".to_owned())),
            headers,
        )
        .await;

        assert_eq!(StatusCode::BAD_GATEWAY, response.status());
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

    async fn upstream_forbidden() -> Response<Body> {
        empty_response(StatusCode::FORBIDDEN)
    }

    async fn upstream_range_ignored() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(
                CONTENT_LENGTH,
                (HLS_INITIALIZATION_SCAN_BYTES + 1).to_string(),
            )
            .body(Body::empty())
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
