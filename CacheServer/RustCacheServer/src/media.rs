use std::{io::SeekFrom, sync::Arc};

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
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::{AppState, hls::HlsMediaResource, library::OpenedMediaFile};

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
    let Some(session) = state.state.hls_sessions.get(&session_id) else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    let body = if head_only {
        Body::empty()
    } else {
        Body::from(session.master_playlist())
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
    let Some(session) = state.state.hls_sessions.get(&session_id) else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    if segment_id.ends_with(".m3u8") {
        let Some(resource) = session.media_playlist_resource(&segment_id) else {
            return empty_response(StatusCode::NOT_FOUND);
        };
        let Ok(initialization) = load_hls_mp4_initialization(&state, &resource).await else {
            return text_response(
                StatusCode::BAD_GATEWAY,
                "HLS upstream MP4 initialization probe failed.\n",
                head_only,
            );
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

    let Some(resource) = session.media_resource(&segment_id) else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    proxy_hls_media_resource(&state, resource, &headers, head_only).await
}

async fn proxy_hls_media_resource(
    state: &MediaState,
    resource: HlsMediaResource,
    headers: &HeaderMap,
    head_only: bool,
) -> Response<Body> {
    let mut urls = Vec::with_capacity(resource.request.backup_urls.len() + 1);
    urls.push(resource.request.url.clone());
    urls.extend(resource.request.backup_urls.clone());

    let mut last_retryable_response = None;
    for url in urls {
        match send_hls_upstream_request(state, &resource, &url, headers, head_only).await {
            Ok(response) if should_retry_hls_upstream_status(response.status()) => {
                last_retryable_response = Some(response);
            }
            Ok(response) => return response,
            Err(_) => continue,
        }
    }

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
    resource: &HlsMediaResource,
    url: &str,
    headers: &HeaderMap,
    head_only: bool,
) -> Result<Response<Body>, reqwest::Error> {
    let method = if head_only { Method::HEAD } else { Method::GET };
    let mut request = hls_upstream_request_builder(state, resource, method, url);
    if let Some(range) = headers.get(RANGE)
        && let Ok(range) = range.to_str()
    {
        request = request.header(RANGE.as_str(), range);
    }

    let upstream = request.send().await?;
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let upstream_headers = upstream.headers().clone();
    let mut response = Response::builder()
        .status(status)
        .body(if head_only {
            Body::empty()
        } else {
            Body::from_stream(upstream.bytes_stream())
        })
        .expect("HLS upstream response should build");

    copy_hls_upstream_headers(
        &upstream_headers,
        response.headers_mut(),
        resource.content_type(),
    );

    Ok(response)
}

fn hls_upstream_request_builder(
    state: &MediaState,
    resource: &HlsMediaResource,
    method: Method,
    url: &str,
) -> reqwest::RequestBuilder {
    let mut request = state.state.hls_upstream_client.request(method, url);
    for header in &resource.request.headers {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Mp4Initialization {
    length: u64,
    total_length: u64,
}

async fn load_hls_mp4_initialization(
    state: &MediaState,
    resource: &HlsMediaResource,
) -> Result<Mp4Initialization, ()> {
    let mut urls = Vec::with_capacity(resource.request.backup_urls.len() + 1);
    urls.push(resource.request.url.clone());
    urls.extend(resource.request.backup_urls.clone());

    for url in urls {
        if let Ok(initialization) =
            load_hls_mp4_initialization_from_url(state, resource, &url).await
        {
            return Ok(initialization);
        }
    }

    Err(())
}

async fn load_hls_mp4_initialization_from_url(
    state: &MediaState,
    resource: &HlsMediaResource,
    url: &str,
) -> Result<Mp4Initialization, ()> {
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
    let total_length = content_range_total_length(&headers).ok_or(())?;
    let bytes = upstream.bytes().await.map_err(|_| ())?;
    let length = mp4_initialization_length(&bytes).ok_or(())?;
    if length == 0 || length >= total_length {
        return Err(());
    }

    Ok(Mp4Initialization {
        length,
        total_length,
    })
}

fn content_range_total_length(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let (_, total) = value.rsplit_once('/')?;
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

fn mp4_initialization_length(bytes: &[u8]) -> Option<u64> {
    let mut offset = 0_usize;
    while offset.checked_add(8)? <= bytes.len() {
        let size32 = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?);
        let box_type = &bytes[offset + 4..offset + 8];
        let (header_length, box_size) = match size32 {
            0 => return None,
            1 => {
                if offset.checked_add(16)? > bytes.len() {
                    return None;
                }
                (
                    16_u64,
                    u64::from_be_bytes(bytes[offset + 8..offset + 16].try_into().ok()?),
                )
            }
            size => (8_u64, u64::from(size)),
        };
        if box_size < header_length {
            return None;
        }
        let end = u64::try_from(offset).ok()?.checked_add(box_size)?;
        if end > u64::try_from(bytes.len()).ok()? {
            return None;
        }
        if box_type == b"moov" {
            return Some(end);
        }
        if matches!(box_type, b"moof" | b"mdat") {
            return None;
        }
        offset = usize::try_from(end).ok()?;
    }

    None
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
        .header(CONTENT_TYPE, opened_file.content_type)
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
        state
            .hls_sessions
            .insert(hls_session("session-1", &upstream_url));
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
        state
            .hls_sessions
            .insert(hls_session("session-1", &upstream_url));

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
        state.hls_sessions.insert(hls_session_with_backups(
            "session-1",
            &primary_url,
            vec![backup_url],
        ));

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
        state
            .hls_sessions
            .insert(hls_session("session-1", &upstream_url));

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

    #[test]
    fn parses_mp4_initialization_length_through_moov_box() {
        let mp4 = fake_mp4();

        assert_eq!(Some(28), mp4_initialization_length(&mp4));
    }

    fn parse(value: &'static str, size: u64) -> Option<ByteRange> {
        parse_range(Some(&HeaderValue::from_static(value)), size).unwrap()
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
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return empty_response(StatusCode::FORBIDDEN);
        }

        let data = fake_mp4();
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
            size: Some(10),
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
