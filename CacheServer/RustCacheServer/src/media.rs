use std::{io::SeekFrom, sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{
        HeaderMap, HeaderValue, Method, Response, StatusCode,
        header::{
            ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
            LAST_MODIFIED, RANGE,
        },
    },
};
use futures_util::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    time::Instant,
};
use tokio_util::io::ReaderStream;

use crate::{
    AppState,
    hls::{HlsMediaResource, mp4_initialization_length, should_forward_media_request_header},
    hls_cache::OpenedPrewarmedHlsResource,
    library::OpenedMediaFile,
};

const HLS_INITIALIZATION_SCAN_BYTES: u64 = 1024 * 1024;

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

fn hls_master_playlist_response(
    state: MediaState,
    session_id: String,
    head_only: bool,
) -> Response<Body> {
    let Some(session) = state.state.hls_playback_session_for_serving(&session_id) else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    let body = if head_only {
        Body::empty()
    } else {
        Body::from(session.master_playlist_with_variant_filter(|variant| {
            state
                .state
                .hls_network_policy
                .variant_is_advertisable(&session_id, &variant.id)
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
    let Some(session) = state.state.hls_playback_session_for_serving(&session_id) else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    if segment_id.ends_with(".m3u8") {
        let Some((variant_id, resource)) =
            session.media_playlist_resource_with_variant(&segment_id)
        else {
            return empty_response(StatusCode::NOT_FOUND);
        };
        let initialization = if let Some(cached) = state
            .state
            .hls_cache
            .cached_resource(&session_id, &resource.id)
        {
            state.state.hls_network_policy.record_cache_hit(&session_id);
            Mp4Initialization {
                length: cached.initialization_length,
                total_length: cached.total_length,
            }
        } else if let Some(prewarmed) = state
            .state
            .hls_cache
            .prewarmed_resource(&session_id, &resource.id)
        {
            state.state.hls_network_policy.record_cache_hit(&session_id);
            Mp4Initialization {
                length: prewarmed.initialization_length,
                total_length: prewarmed.total_length,
            }
        } else {
            let Ok(initialization) =
                load_hls_mp4_initialization(&state, &session_id, &variant_id, &resource).await
            else {
                state
                    .state
                    .hls_network_policy
                    .record_upstream_failure(&session_id, &variant_id);
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    "HLS upstream MP4 initialization probe failed.\n",
                    head_only,
                );
            };
            initialization
        };
        let Some(playlist) = session.media_playlist(
            &segment_id,
            initialization.length,
            initialization.total_length,
        ) else {
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

    let Some((variant_id, resource)) = session.media_resource_with_variant(&segment_id) else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    if let Some(opened_file) = state
        .state
        .hls_cache
        .open_cached_resource(&session_id, &resource.id)
    {
        state.state.hls_network_policy.record_cache_hit(&session_id);
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

    if let Some(opened_file) = state
        .state
        .hls_cache
        .open_prewarmed_resource(&session_id, &resource.id)
        && let Some(range_header) = headers.get(RANGE)
    {
        let range = match parse_range(Some(range_header), opened_file.total_length) {
            Ok(Some(range)) if range.end < opened_file.prefix_length => {
                state.state.hls_network_policy.record_cache_hit(&session_id);
                range
            }
            Ok(_) => {
                return proxy_hls_media_resource(
                    &state,
                    &session_id,
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
        &session_id,
        &variant_id,
        resource,
        &headers,
        head_only,
    )
    .await
}

async fn proxy_hls_media_resource(
    state: &MediaState,
    session_id: &str,
    variant_id: &str,
    resource: HlsMediaResource,
    headers: &HeaderMap,
    head_only: bool,
) -> Response<Body> {
    let mut urls = Vec::with_capacity(resource.request.backup_urls.len() + 1);
    urls.push(resource.request.url.clone());
    urls.extend(resource.request.backup_urls.clone());

    let mut last_retryable_response = None;
    for url in urls {
        match send_hls_upstream_request(
            state, session_id, variant_id, &resource, &url, headers, head_only,
        )
        .await
        {
            Ok(upstream) if should_retry_hls_upstream_status(upstream.response.status()) => {
                state
                    .state
                    .hls_network_policy
                    .record_upstream_retry(session_id, variant_id);
                last_retryable_response = Some(upstream.response);
            }
            Ok(upstream) => return upstream.response,
            Err(_) => {
                state
                    .state
                    .hls_network_policy
                    .record_upstream_retry(session_id, variant_id);
                continue;
            }
        }
    }

    state
        .state
        .hls_network_policy
        .record_upstream_failure(session_id, variant_id);
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
    state: &MediaState,
    session_id: &str,
    variant_id: &str,
    resource: &HlsMediaResource,
    url: &str,
    headers: &HeaderMap,
    head_only: bool,
) -> Result<HlsUpstreamResponse, reqwest::Error> {
    let method = if head_only { Method::HEAD } else { Method::GET };
    let mut request = hls_upstream_request_builder(state, resource, method, url);
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
                state.state.hls_network_policy.record_upstream_success(
                    session_id,
                    variant_id,
                    response_time,
                );
            }
            Body::empty()
        } else if status.is_success() {
            hls_policy_recording_body(
                upstream,
                state.state.hls_network_policy.clone(),
                session_id.to_owned(),
                variant_id.to_owned(),
                started_at,
            )
        } else {
            Body::from_stream(upstream.bytes_stream())
        })
        .expect("HLS upstream response should build");

    copy_hls_upstream_headers(
        &upstream_headers,
        response.headers_mut(),
        resource.content_type(),
    );

    Ok(HlsUpstreamResponse { response })
}

struct HlsUpstreamResponse {
    response: Response<Body>,
}

fn hls_policy_recording_body(
    upstream: reqwest::Response,
    policy: crate::hls_network_policy::HlsNetworkPolicy,
    session_id: String,
    variant_id: String,
    started_at: Instant,
) -> Body {
    let stream = Box::pin(upstream.bytes_stream());
    let stream = futures_util::stream::unfold(
        (stream, policy, session_id, variant_id, started_at, false),
        |(mut stream, policy, session_id, variant_id, started_at, failed)| async move {
            match stream.next().await {
                Some(Ok(bytes)) => Some((
                    Ok::<_, reqwest::Error>(bytes),
                    (stream, policy, session_id, variant_id, started_at, failed),
                )),
                Some(Err(error)) => {
                    if !failed {
                        policy.record_upstream_failure(&session_id, &variant_id);
                    }
                    Some((
                        Err(error),
                        (stream, policy, session_id, variant_id, started_at, true),
                    ))
                }
                None => {
                    if !failed {
                        policy.record_upstream_success(
                            &session_id,
                            &variant_id,
                            started_at.elapsed(),
                        );
                    }
                    None
                }
            }
        },
    );
    Body::from_stream(stream)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Mp4Initialization {
    length: u64,
    total_length: u64,
}

struct Mp4InitializationProbe {
    initialization: Mp4Initialization,
    response_time: Duration,
}

async fn load_hls_mp4_initialization(
    state: &MediaState,
    session_id: &str,
    variant_id: &str,
    resource: &HlsMediaResource,
) -> Result<Mp4Initialization, ()> {
    let mut urls = Vec::with_capacity(resource.request.backup_urls.len() + 1);
    urls.push(resource.request.url.clone());
    urls.extend(resource.request.backup_urls.clone());

    for url in urls {
        match load_hls_mp4_initialization_from_url(state, resource, &url).await {
            Ok(probe) => {
                state.state.hls_network_policy.record_upstream_success(
                    session_id,
                    variant_id,
                    probe.response_time,
                );
                return Ok(probe.initialization);
            }
            Err(()) => state
                .state
                .hls_network_policy
                .record_upstream_retry(session_id, variant_id),
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
        },
        response_time: started_at.elapsed(),
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
    HeaderValue::from_str(value)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
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
    use super::*;
    use axum::{Router, body::to_bytes, routing::get};
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
            PlaybackProtocol, PlaybackSource, TaskState,
        },
        hls::{HlsMediaResource, HlsPlaybackSession, HlsVariant},
    };

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
    async fn hls_media_playlist_keeps_single_media_range_for_prewarmed_prefix() {
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
        let mp4 = large_fake_mp4();
        let initialization_length = mp4_initialization_length(&mp4).unwrap();
        let media_length = u64::try_from(mp4.len()).unwrap() - initialization_length;
        assert!(playlist.contains(&format!(
            "#EXT-X-BYTERANGE:{media_length}@{initialization_length}"
        )));
        assert!(!playlist.contains(&format!("@{HLS_INITIALIZATION_SCAN_BYTES}")));
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

    fn insert_authorized_hls_session(state: &AppState, session: HlsPlaybackSession) {
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
        };
        state.hls_sessions.insert(session);
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
                    id: session_id,
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
