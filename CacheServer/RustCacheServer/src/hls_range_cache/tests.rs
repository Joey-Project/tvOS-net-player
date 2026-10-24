use std::{
    io::{Seek, SeekFrom, Write},
    ops::Range,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode, header::CONTENT_RANGE},
    routing::get,
};
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle, time::timeout};

use crate::{
    bbdown_adapter::{BilibiliMediaCacheKey, BilibiliMediaRequest, BilibiliMediaRequestKind},
    hls::HlsMediaResource,
    hls_cache::{HlsCacheFillControl, HlsCacheStore, ensure_range_manifest_capacity},
};

use super::*;

const TEST_DEADLINE: Duration = Duration::from_secs(8);

struct MockRangeState {
    bytes: Arc<Vec<u8>>,
    requests: Mutex<Vec<Range<usize>>>,
    fail_once: Option<Range<usize>>,
    failed: AtomicBool,
    truncate_once: Option<Range<usize>>,
    truncated: AtomicBool,
    stalled_body_once: Option<Range<usize>>,
    stalled_body: AtomicBool,
    body_entered: Arc<Notify>,
    release_body: Arc<Notify>,
}

async fn start_range_server(
    bytes: Vec<u8>,
    fail_once: Option<Range<usize>>,
) -> (String, Arc<MockRangeState>, JoinHandle<()>) {
    start_range_server_with_behavior(bytes, fail_once, None, None).await
}

async fn start_range_server_with_behavior(
    bytes: Vec<u8>,
    fail_once: Option<Range<usize>>,
    truncate_once: Option<Range<usize>>,
    stalled_body_once: Option<Range<usize>>,
) -> (String, Arc<MockRangeState>, JoinHandle<()>) {
    let state = Arc::new(MockRangeState {
        bytes: Arc::new(bytes),
        requests: Mutex::new(Vec::new()),
        fail_once,
        failed: AtomicBool::new(false),
        truncate_once,
        truncated: AtomicBool::new(false),
        stalled_body_once,
        stalled_body: AtomicBool::new(false),
        body_entered: Arc::new(Notify::new()),
        release_body: Arc::new(Notify::new()),
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener should bind");
    let address = listener
        .local_addr()
        .expect("loopback listener should have an address");
    let app = Router::new()
        .route("/media", get(serve_range))
        .with_state(Arc::clone(&state));
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("loopback range server should remain available");
    });
    (format!("http://{address}/media"), state, task)
}

async fn serve_range(
    State(state): State<Arc<MockRangeState>>,
    headers: HeaderMap,
) -> Response<Body> {
    let Some(value) = headers
        .get(axum::http::header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
    else {
        return response(StatusCode::BAD_REQUEST, None, Vec::new());
    };
    let Some((start, inclusive_end)) = value.split_once('-') else {
        return response(StatusCode::BAD_REQUEST, None, Vec::new());
    };
    let (Ok(start), Ok(inclusive_end)) = (start.parse::<usize>(), inclusive_end.parse::<usize>())
    else {
        return response(StatusCode::BAD_REQUEST, None, Vec::new());
    };
    let Some(end) = inclusive_end.checked_add(1) else {
        return response(StatusCode::BAD_REQUEST, None, Vec::new());
    };
    let requested = start..end;
    state
        .requests
        .lock()
        .expect("mock request list should not poison")
        .push(requested.clone());
    if state.fail_once.as_ref() == Some(&requested) && !state.failed.swap(true, Ordering::SeqCst) {
        return response(StatusCode::SERVICE_UNAVAILABLE, None, Vec::new());
    }
    if requested.start >= requested.end || requested.end > state.bytes.len() {
        return response(StatusCode::RANGE_NOT_SATISFIABLE, None, Vec::new());
    }
    if state.truncate_once.as_ref() == Some(&requested)
        && !state.truncated.swap(true, Ordering::SeqCst)
    {
        return response(
            StatusCode::PARTIAL_CONTENT,
            Some(format!(
                "bytes {}-{}/{}",
                requested.start,
                requested.end - 1,
                state.bytes.len()
            )),
            state.bytes[requested.start..requested.end - 1].to_vec(),
        );
    }
    if state.stalled_body_once.as_ref() == Some(&requested)
        && !state.stalled_body.swap(true, Ordering::SeqCst)
    {
        let body = state.bytes[requested.clone()].to_vec();
        let body_entered = state.body_entered.clone();
        let release_body = state.release_body.clone();
        let stream = futures_util::stream::once(async move {
            body_entered.notify_one();
            release_body.notified().await;
            Ok::<_, std::io::Error>(axum::body::Bytes::from(body))
        });
        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                axum::http::header::CONTENT_LENGTH,
                (requested.end - requested.start).to_string(),
            )
            .header(
                CONTENT_RANGE,
                format!(
                    "bytes {}-{}/{}",
                    requested.start,
                    requested.end - 1,
                    state.bytes.len()
                ),
            )
            .body(Body::from_stream(stream))
            .expect("stalled range response should be valid");
    }
    response(
        StatusCode::PARTIAL_CONTENT,
        Some(format!(
            "bytes {}-{}/{}",
            requested.start,
            requested.end - 1,
            state.bytes.len()
        )),
        state.bytes[requested].to_vec(),
    )
}

fn response(status: StatusCode, content_range: Option<String>, body: Vec<u8>) -> Response<Body> {
    let mut builder = Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_LENGTH, body.len().to_string());
    if let Some(value) = content_range {
        builder = builder.header(CONTENT_RANGE, value);
    }
    builder
        .body(Body::from(body))
        .expect("mock response should be valid")
}

async fn stop_server(task: JoinHandle<()>) {
    task.abort();
    let _ = timeout(Duration::from_secs(1), task).await;
}

fn resource(url: String, size: Option<u64>) -> HlsMediaResource {
    HlsMediaResource {
        id: "video-resource".to_owned(),
        request: BilibiliMediaRequest {
            kind: BilibiliMediaRequestKind::Video,
            stream_id: Some(80),
            url,
            backup_urls: Vec::new(),
            headers: Vec::new(),
            mime_type: Some("application/octet-stream".to_owned()),
            codecs: None,
            bandwidth: None,
            width: None,
            height: None,
            frame_rate: None,
            size,
            duration_seconds: None,
            cache_key: BilibiliMediaCacheKey {
                content_id: "test-content".to_owned(),
                media_kind: BilibiliMediaRequestKind::Video,
                stream_id: Some(80),
                codecs: None,
                source_hash: "test-source".to_owned(),
            },
        },
    }
}

fn patterned_bytes(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| index.wrapping_mul(31).wrapping_add(7) as u8)
        .collect()
}

fn test_store(path: &std::path::Path) -> HlsCacheStore {
    HlsCacheStore::new(
        path.canonicalize()
            .expect("owned temporary cache root should canonicalize"),
    )
    .with_range_budget(8 * 1024 * 1024)
    .with_range_parallelism(2)
}

async fn seed_startup_range(
    store: &HlsCacheStore,
    client: &reqwest::Client,
    session_id: &str,
    resource: &HlsMediaResource,
) {
    store
        .ensure_resource_range(
            client,
            session_id,
            resource,
            0..1,
            HlsRangePriority::Foreground,
            &|| HlsCacheFillControl::Continue,
        )
        .await
        .expect("startup range should be checkpointed");
}

#[tokio::test]
async fn restart_reads_checkpointed_bytes_and_requests_only_missing_extent() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let missing = RANGE_STARTUP_CHUNK_BYTES as usize..total;
        let (url, state, server) = start_range_server(bytes.clone(), Some(missing.clone())).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let resource = resource(url, Some(total as u64));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("test client should build");

        let first_store = test_store(temp.path());
        let first_fill = first_store
            .fill_missing_resource_ranges(
                &client,
                "partial-restart",
                &resource,
                HlsRangePriority::Background,
                &|| HlsCacheFillControl::Continue,
            )
            .await;
        assert!(matches!(first_fill, Err(HlsRangeError::UpstreamStatus(_))));
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES,
            first_store
                .range_durable_bytes("partial-restart", &resource)
                .expect("partial checkpoint should be readable")
        );
        drop(first_store);

        let restarted = test_store(temp.path());
        let selected = 11_u64..129;
        assert_eq!(
            bytes[selected.start as usize..selected.end as usize],
            restarted
                .read_resource_range("partial-restart", &resource, selected.clone())
                .await
                .expect("checkpointed range should read after restart")
                .expect("checkpointed range should be present")
        );
        let after_read = state
            .requests
            .lock()
            .expect("request list should not poison")
            .clone();
        assert_eq!(
            vec![0..RANGE_STARTUP_CHUNK_BYTES as usize, missing.clone()],
            after_read
        );

        let status = restarted
            .fill_missing_resource_ranges(
                &client,
                "partial-restart",
                &resource,
                HlsRangePriority::Background,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("restart should fill the missing extent");
        assert_eq!(
            HlsRangeResourceStatus::Complete {
                total_length: total as u64
            },
            status
        );
        let requests = state
            .requests
            .lock()
            .expect("request list should not poison")
            .clone();
        assert_eq!(
            1,
            requests
                .iter()
                .filter(|range| {
                    range.start == 0 && range.end == RANGE_STARTUP_CHUNK_BYTES as usize
                })
                .count()
        );
        assert_eq!(
            2,
            requests
                .iter()
                .filter(|range| range.start == missing.start && range.end == missing.end)
                .count()
        );
        assert_eq!(
            total as u64,
            restarted
                .range_durable_bytes("partial-restart", &resource)
                .expect("completed extent checkpoints should be readable")
        );
        stop_server(server).await;
    })
    .await
    .expect("partial restart test should finish within its deadline");
}

#[tokio::test]
async fn empty_unknown_size_range_does_not_overlap_or_fetch() {
    timeout(TEST_DEADLINE, async {
        let (url, state, server) = start_range_server(patterned_bytes(1024), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url, None);
        let error = store
            .ensure_resource_range(
                &reqwest::Client::new(),
                "empty-unknown-size",
                &resource,
                0..0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect_err("empty range should be rejected before unknown-size probing");
        assert!(matches!(error, HlsRangeError::InvalidResponse(_)));
        assert!(
            state
                .requests
                .lock()
                .expect("request list should not poison")
                .is_empty()
        );
        assert_eq!(
            0,
            store
                .range_durable_bytes("empty-unknown-size", &resource)
                .unwrap()
        );
        stop_server(server).await;
    })
    .await
    .expect("empty unknown-size range test should finish within its deadline");
}

#[tokio::test]
async fn unknown_size_probe_is_not_checkpointed_and_only_missing_tail_is_filled() {
    timeout(TEST_DEADLINE, async {
        let total = RANGE_STARTUP_CHUNK_BYTES as usize + 137;
        let bytes = patterned_bytes(total);
        let (url, state, server) = start_range_server(bytes.clone(), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url, None);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("test client should build");

        let ready = store
            .ensure_resource_range(
                &client,
                "unknown-size-probe",
                &resource,
                0..RANGE_STARTUP_CHUNK_BYTES,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("unknown-size resource should discover its length and cache prefix");
        assert_eq!(total as u64, ready.total_length);
        assert_eq!(
            vec![0..1, 0..RANGE_STARTUP_CHUNK_BYTES as usize],
            state
                .requests
                .lock()
                .expect("request list should not poison")
                .clone()
        );

        let manifest_path = temp
            .path()
            .join(".tvos-net-player/hls/unknown-size-probe/video-resource.range.json");
        let manifest_bytes = std::fs::read(manifest_path)
            .expect("owned temporary range manifest should be readable");
        let manifest: PersistedRangeManifest =
            serde_json::from_slice(&manifest_bytes).expect("range manifest should deserialize");
        assert_eq!(Some(total as u64), manifest.total_length);
        assert_eq!(RANGE_STARTUP_CHUNK_BYTES, manifest.durable_bytes);
        assert_eq!(1, manifest.extents.len());
        assert_eq!(0, manifest.extents[0].start);
        assert_eq!(RANGE_STARTUP_CHUNK_BYTES, manifest.extents[0].end);
        assert_eq!(
            bytes[..RANGE_STARTUP_CHUNK_BYTES as usize],
            store
                .read_resource_range(
                    "unknown-size-probe",
                    &resource,
                    0..RANGE_STARTUP_CHUNK_BYTES,
                )
                .await
                .expect("checkpointed prefix should read")
                .expect("prefix should be durable")
        );

        assert_eq!(
            HlsRangeResourceStatus::Complete {
                total_length: total as u64
            },
            store
                .fill_missing_resource_ranges(
                    &client,
                    "unknown-size-probe",
                    &resource,
                    HlsRangePriority::Background,
                    &|| HlsCacheFillControl::Continue,
                )
                .await
                .expect("fill should request only the uncheckpointed tail")
        );
        assert_eq!(
            vec![
                0..1,
                0..RANGE_STARTUP_CHUNK_BYTES as usize,
                RANGE_STARTUP_CHUNK_BYTES as usize..total
            ],
            state
                .requests
                .lock()
                .expect("request list should not poison")
                .clone()
        );
        assert_eq!(
            total as u64,
            store
                .range_durable_bytes("unknown-size-probe", &resource)
                .unwrap()
        );
        stop_server(server).await;
    })
    .await
    .expect("unknown-size probe test should finish within its deadline");
}

#[tokio::test]
async fn foreground_ranges_are_64k_aligned_and_partial_cache_restores() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let (url, state, server) = start_range_server(bytes.clone(), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let resource = resource(url, Some(total as u64));
        let client = reqwest::Client::new();
        let first_store = test_store(temp.path()).with_range_parallelism(1);
        let selected = RANGE_STARTUP_CHUNK_BYTES + 123
            ..RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES + 321;

        first_store
            .ensure_resource_range(
                &client,
                "foreground-64k-restore",
                &resource,
                selected.clone(),
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("bounded foreground spans should become ready");
        assert_eq!(
            vec![
                0..RANGE_STARTUP_CHUNK_BYTES as usize,
                RANGE_STARTUP_CHUNK_BYTES as usize
                    ..(RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES) as usize,
                (RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES) as usize
                    ..(RANGE_STARTUP_CHUNK_BYTES + 2 * RANGE_FOREGROUND_CHUNK_BYTES) as usize,
            ],
            state
                .requests
                .lock()
                .expect("request list should not poison")
                .clone()
        );
        assert!(
            state
                .requests
                .lock()
                .expect("request list should not poison")
                .iter()
                .skip(1)
                .all(|range| range.end - range.start <= RANGE_FOREGROUND_CHUNK_BYTES as usize)
        );
        let partial_manifest = first_store
            .range_manifest_snapshot("foreground-64k-restore", &resource)
            .expect("fragmented partial manifest should validate")
            .expect("fragmented partial manifest should exist");
        assert_eq!(3, partial_manifest.extents.len());
        assert_eq!(0, partial_manifest.extents[0].start);
        assert_eq!(RANGE_STARTUP_CHUNK_BYTES, partial_manifest.extents[0].end);
        assert_eq!(RANGE_STARTUP_CHUNK_BYTES, partial_manifest.extents[1].start);
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES,
            partial_manifest.extents[1].end
        );
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES,
            partial_manifest.extents[2].start
        );
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES + 2 * RANGE_FOREGROUND_CHUNK_BYTES,
            partial_manifest.extents[2].end
        );
        assert!(
            first_store
                .range_durable_bytes("foreground-64k-restore", &resource)
                .expect("partial checkpoint should be readable")
                < total as u64
        );
        drop(first_store);

        let restarted = test_store(temp.path());
        assert_eq!(
            bytes[selected.start as usize..selected.end as usize],
            restarted
                .read_resource_range("foreground-64k-restore", &resource, selected.clone())
                .await
                .expect("restored small ranges should validate")
                .expect("restored small ranges should be present")
        );
        assert_eq!(
            HlsRangeResourceStatus::Complete {
                total_length: total as u64
            },
            restarted
                .fill_missing_resource_ranges(
                    &client,
                    "foreground-64k-restore",
                    &resource,
                    HlsRangePriority::Background,
                    &|| HlsCacheFillControl::Continue,
                )
                .await
                .expect("background should complete the restored partial cache")
        );
        let completed_manifest = restarted
            .range_manifest_snapshot("foreground-64k-restore", &resource)
            .expect("completed manifest should validate")
            .expect("completed manifest should exist");
        assert_eq!(2, completed_manifest.extents.len());
        assert_eq!(total as u64, completed_manifest.extents[1].end);
        let requests = state
            .requests
            .lock()
            .expect("request list should not poison")
            .clone();
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES as usize + 2 * RANGE_FOREGROUND_CHUNK_BYTES as usize,
            requests
                .last()
                .expect("background gap should be requested")
                .start
        );
        assert_eq!(
            total,
            requests
                .last()
                .expect("background gap should be requested")
                .end
        );
        stop_server(server).await;
    })
    .await
    .expect("foreground restore test should finish within its deadline");
}

#[tokio::test]
async fn overlapping_foreground_commits_accept_both_completion_orders() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let (url, _state, server) = start_range_server(bytes.clone(), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url.clone(), Some(total as u64));
        let client = reqwest::Client::new();
        let first =
            RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES;
        let second = RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES / 2
            ..RANGE_STARTUP_CHUNK_BYTES
                + RANGE_FOREGROUND_CHUNK_BYTES
                + RANGE_FOREGROUND_CHUNK_BYTES / 2;

        for (session_id, ranges) in [
            ("overlap-first-a", [&first, &second]),
            ("overlap-first-b", [&second, &first]),
        ] {
            seed_startup_range(&store, &client, session_id, &resource).await;
            let expected = store
                .range_manifest_snapshot(session_id, &resource)
                .expect("seed checkpoint should validate")
                .expect("seed checkpoint should exist");
            for range in ranges {
                store
                    .commit_range_extent(
                        session_id,
                        &resource,
                        &expected,
                        range.clone(),
                        &bytes[range.start as usize..range.end as usize],
                        &url,
                        None,
                        None,
                    )
                    .await
                    .expect("matching overlap should publish in either order");
            }
            let selected = first.start..second.end;
            assert_eq!(
                bytes[selected.start as usize..selected.end as usize],
                store
                    .read_resource_range(session_id, &resource, selected.clone())
                    .await
                    .expect("coalesced overlap should validate")
                    .expect("both overlapping ranges should be durable")
            );
        }
        stop_server(server).await;
    })
    .await
    .expect("overlap ordering test should finish within its deadline");
}

#[tokio::test]
async fn conflicting_overlap_preserves_published_bytes_and_missing_gap() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let (url, _state, server) = start_range_server(bytes.clone(), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url.clone(), Some(total as u64));
        let client = reqwest::Client::new();
        let session_id = "conflicting-overlap";
        seed_startup_range(&store, &client, session_id, &resource).await;
        let expected = store
            .range_manifest_snapshot(session_id, &resource)
            .expect("seed checkpoint should validate")
            .expect("seed checkpoint should exist");
        let first =
            RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES;
        let second = RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES / 2
            ..RANGE_STARTUP_CHUNK_BYTES
                + RANGE_FOREGROUND_CHUNK_BYTES
                + RANGE_FOREGROUND_CHUNK_BYTES / 2;
        store
            .commit_range_extent(
                session_id,
                &resource,
                &expected,
                first.clone(),
                &bytes[first.start as usize..first.end as usize],
                &url,
                None,
                None,
            )
            .await
            .expect("first trusted range should publish");
        let mut conflicting = bytes[second.start as usize..second.end as usize].to_vec();
        conflicting[0] ^= 1;
        let error = store
            .commit_range_extent(
                session_id,
                &resource,
                &expected,
                second.clone(),
                &conflicting,
                &url,
                None,
                None,
            )
            .await
            .expect_err("conflicting overlap must not overwrite trusted bytes");
        assert!(matches!(error, HlsRangeError::IdentityChanged));
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES,
            store
                .range_durable_bytes(session_id, &resource)
                .expect("old checkpoint should remain readable")
        );
        assert_eq!(
            bytes[first.start as usize..first.end as usize],
            store
                .read_resource_range(session_id, &resource, first.clone())
                .await
                .expect("old bytes should remain valid")
                .expect("old bytes should remain durable")
        );
        assert_eq!(
            None,
            store
                .read_resource_range(session_id, &resource, second)
                .await
                .expect("partially covered range should remain a cache miss")
        );
        stop_server(server).await;
    })
    .await
    .expect("conflicting overlap test should finish within its deadline");
}

#[tokio::test]
async fn same_inode_old_extent_tamper_blocks_merge() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let (url, _state, server) = start_range_server(bytes.clone(), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url.clone(), Some(total as u64));
        let client = reqwest::Client::new();
        let session_id = "old-extent-tamper";
        seed_startup_range(&store, &client, session_id, &resource).await;
        let expected = store
            .range_manifest_snapshot(session_id, &resource)
            .expect("seed checkpoint should validate")
            .expect("seed checkpoint should exist");
        let first =
            RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES;
        let middle = first.end..total as u64 - RANGE_FOREGROUND_CHUNK_BYTES;
        let last = middle.end..total as u64;
        store
            .commit_range_extent(
                session_id,
                &resource,
                &expected,
                first.clone(),
                &bytes[first.start as usize..first.end as usize],
                &url,
                None,
                None,
            )
            .await
            .expect("first trusted range should publish");
        let expected = store
            .range_manifest_snapshot(session_id, &resource)
            .expect("first fragment checkpoint should validate")
            .expect("first fragment checkpoint should exist");
        store
            .commit_range_extent(
                session_id,
                &resource,
                &expected,
                middle.clone(),
                &bytes[middle.start as usize..middle.end as usize],
                &url,
                None,
                None,
            )
            .await
            .expect("middle fragment should leave only the tail missing");
        let data_path = temp
            .path()
            .join(".tvos-net-player/hls/old-extent-tamper/video-resource.range.data");
        let mut data = std::fs::OpenOptions::new()
            .write(true)
            .open(data_path)
            .expect("range data should open without replacing its object");
        data.seek(SeekFrom::Start(first.start))
            .expect("existing range offset should seek");
        data.write_all(&[bytes[first.start as usize] ^ 1])
            .expect("same-inode content mutation should write");
        data.sync_all().expect("tampered byte should flush");
        let expected = store
            .range_manifest_snapshot(session_id, &resource)
            .expect("fragmented checkpoint should validate")
            .expect("fragmented checkpoint should exist");
        let key = RangeChunkKey {
            resource: RangeResourceKey {
                session_id: session_id.to_owned(),
                resource_id: resource.id.clone(),
                representation: expected.representation_digest.clone(),
            },
            total_length: total as u64,
            range: last.clone(),
        };
        let flight = store
            .range_cache_for_test()
            .claim_chunk(key, HlsRangePriority::Foreground)
            .expect("tail flight should claim");
        assert!(flight.is_owner());
        let error = store
            .commit_range_extent(
                session_id,
                &resource,
                &expected,
                last.clone(),
                &bytes[last.start as usize..last.end as usize],
                &url,
                None,
                None,
            )
            .await
            .expect_err("old extent hash mismatch must block compaction");
        drop(flight);
        assert!(matches!(error, HlsRangeError::IdentityChanged));
        assert_eq!(
            total as u64 - RANGE_FOREGROUND_CHUNK_BYTES,
            store
                .range_durable_bytes(session_id, &resource)
                .expect("old checkpoint should remain readable")
        );
        assert_eq!(
            None,
            store
                .read_resource_range(session_id, &resource, last)
                .await
                .expect("unpublished merged range should remain a cache miss")
        );
        stop_server(server).await;
    })
    .await
    .expect("tampered merge test should finish within its deadline");
}

#[tokio::test]
async fn failed_metadata_checkpoint_keeps_old_manifest_and_allows_retry() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let (url, _state, server) = start_range_server(bytes.clone(), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url.clone(), Some(total as u64));
        let client = reqwest::Client::new();
        let session_id = "checkpoint-retry";
        seed_startup_range(&store, &client, session_id, &resource).await;
        let expected = store
            .range_manifest_snapshot(session_id, &resource)
            .expect("seed checkpoint should validate")
            .expect("seed checkpoint should exist");
        let range = RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES;
        let manifest_tmp = temp
            .path()
            .join(".tvos-net-player/hls/checkpoint-retry/video-resource.range.tmp");
        std::fs::create_dir(&manifest_tmp)
            .expect("non-file temp path should make checkpoint publication fail");
        let error = store
            .commit_range_extent(
                session_id,
                &resource,
                &expected,
                range.clone(),
                &bytes[range.start as usize..range.end as usize],
                &url,
                None,
                None,
            )
            .await
            .expect_err("metadata checkpoint failure should not publish the range");
        assert!(matches!(error, HlsRangeError::Io(ref error) if error.kind() == std::io::ErrorKind::AlreadyExists));
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES,
            store
                .range_durable_bytes(session_id, &resource)
                .expect("previous checkpoint should remain readable")
        );
        assert_eq!(
            None,
            store
                .read_resource_range(session_id, &resource, range.clone())
                .await
                .expect("uncheckpointed bytes should remain unavailable")
        );
        std::fs::remove_dir(manifest_tmp).expect("temporary blocker should be removed");
        store
            .commit_range_extent(
                session_id,
                &resource,
                &expected,
                range.clone(),
                &bytes[range.start as usize..range.end as usize],
                &url,
                None,
                None,
            )
            .await
            .expect("valid retry should publish the missing range");
        assert_eq!(
            bytes[range.start as usize..range.end as usize],
            store
                .read_resource_range(session_id, &resource, range)
                .await
                .expect("retried checkpoint should validate")
                .expect("retried bytes should be durable")
        );
        stop_server(server).await;
    })
    .await
    .expect("checkpoint failure test should finish within its deadline");
}

#[tokio::test]
async fn truncated_foreground_response_is_not_published_and_valid_retry_succeeds() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let requested = RANGE_STARTUP_CHUNK_BYTES as usize
            ..(RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES) as usize;
        let (url, state, server) =
            start_range_server_with_behavior(bytes.clone(), None, Some(requested.clone()), None)
                .await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url, Some(total as u64));
        let client = reqwest::Client::new();
        let session_id = "truncated-range-retry";
        seed_startup_range(&store, &client, session_id, &resource).await;
        let selected = RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + 1;
        let error = store
            .ensure_resource_range(
                &client,
                session_id,
                &resource,
                selected.clone(),
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect_err("truncated range body must fail validation");
        assert!(matches!(error, HlsRangeError::InvalidResponse(_)));
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES,
            store
                .range_durable_bytes(session_id, &resource)
                .expect("startup checkpoint should remain readable")
        );
        assert_eq!(
            None,
            store
                .read_resource_range(session_id, &resource, selected.clone())
                .await
                .expect("truncated response must leave no published bytes")
        );
        store
            .ensure_resource_range(
                &client,
                session_id,
                &resource,
                selected.clone(),
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("a complete retry should publish the range");
        assert_eq!(
            bytes[selected.start as usize..selected.end as usize],
            store
                .read_resource_range(session_id, &resource, selected)
                .await
                .expect("retry checkpoint should validate")
                .expect("retry bytes should be durable")
        );
        assert!(state.truncated.load(Ordering::SeqCst));
        stop_server(server).await;
    })
    .await
    .expect("truncated response test should finish within its deadline");
}

#[tokio::test]
async fn cancelled_foreground_body_releases_flight_for_valid_retry() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let requested = RANGE_STARTUP_CHUNK_BYTES as usize
            ..(RANGE_STARTUP_CHUNK_BYTES + RANGE_FOREGROUND_CHUNK_BYTES) as usize;
        let (url, state, server) =
            start_range_server_with_behavior(bytes.clone(), None, None, Some(requested)).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url, Some(total as u64));
        let client = reqwest::Client::new();
        let session_id = "cancelled-range-retry";
        seed_startup_range(&store, &client, session_id, &resource).await;
        let selected = RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_store = store.clone();
        let task_resource = resource.clone();
        let task_client = client.clone();
        let task_cancelled = Arc::clone(&cancelled);
        let task = tokio::spawn(async move {
            task_store
                .ensure_resource_range(
                    &task_client,
                    session_id,
                    &task_resource,
                    selected,
                    HlsRangePriority::Foreground,
                    &|| {
                        if task_cancelled.load(Ordering::SeqCst) {
                            HlsCacheFillControl::Cancel
                        } else {
                            HlsCacheFillControl::Continue
                        }
                    },
                )
                .await
        });
        timeout(Duration::from_secs(2), state.body_entered.notified())
            .await
            .expect("foreground response body should reach its stall");
        cancelled.store(true, Ordering::SeqCst);
        let outcome = timeout(Duration::from_secs(2), task)
            .await
            .expect("cancel should stop the body read promptly")
            .expect("cancelled foreground task should join");
        assert!(matches!(outcome, Err(HlsRangeError::Cancelled)));
        state.release_body.notify_one();
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES,
            store
                .range_durable_bytes("cancelled-range-retry", &resource)
                .expect("cancel should leave the prior checkpoint intact")
        );
        store
            .ensure_resource_range(
                &client,
                "cancelled-range-retry",
                &resource,
                RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + 1,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("retry should acquire a released flight and permit");
        assert_eq!(
            bytes[RANGE_STARTUP_CHUNK_BYTES as usize],
            store
                .read_resource_range(
                    "cancelled-range-retry",
                    &resource,
                    RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + 1,
                )
                .await
                .expect("retry bytes should validate")
                .expect("retry byte should be durable")[0]
        );
        stop_server(server).await;
    })
    .await
    .expect("cancel/retry test should finish within its deadline");
}

#[tokio::test]
async fn foreground_preempts_stalled_background_body_with_one_permit() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 2) as usize;
        let bytes = patterned_bytes(total);
        let background_range = RANGE_STARTUP_CHUNK_BYTES as usize..total;
        let (url, state, server) =
            start_range_server_with_behavior(bytes.clone(), None, None, Some(background_range))
                .await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path()).with_range_parallelism(1);
        let resource = resource(url, Some(total as u64));
        let client = reqwest::Client::new();
        let session_id = "one-permit-preemption";
        seed_startup_range(&store, &client, session_id, &resource).await;

        let background_store = store.clone();
        let background_resource = resource.clone();
        let background_client = client.clone();
        let background = tokio::spawn(async move {
            background_store
                .fill_missing_resource_ranges(
                    &background_client,
                    session_id,
                    &background_resource,
                    HlsRangePriority::Background,
                    &|| HlsCacheFillControl::Continue,
                )
                .await
        });
        timeout(Duration::from_secs(2), state.body_entered.notified())
            .await
            .expect("background response body should stall while holding the only permit");

        let selected = RANGE_STARTUP_CHUNK_BYTES + 19..RANGE_STARTUP_CHUNK_BYTES + 37;
        timeout(
            Duration::from_secs(3),
            store.ensure_resource_range(
                &client,
                session_id,
                &resource,
                selected.clone(),
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            ),
        )
        .await
        .expect("foreground request should progress before the background body resumes")
        .expect("foreground small range should be ready");
        state.release_body.notify_one();
        let background_result = timeout(Duration::from_secs(2), background)
            .await
            .expect("preempted background task should finish")
            .expect("background task should join");
        assert!(matches!(background_result, Err(HlsRangeError::Preempted)));
        assert_eq!(
            bytes[selected.start as usize..selected.end as usize],
            store
                .read_resource_range(session_id, &resource, selected)
                .await
                .expect("foreground range should validate")
                .expect("foreground range should be durable")
        );
        stop_server(server).await;
    })
    .await
    .expect("single-permit preemption test should finish within its deadline");
}

#[tokio::test]
async fn extent_cap_admission_precedes_data_writes() {
    timeout(TEST_DEADLINE, async {
        let total = RANGE_STARTUP_CHUNK_BYTES as usize + 2 * RANGE_MAX_CHUNKS as usize;
        let bytes = patterned_bytes(total);
        let (url, _state, server) = start_range_server(bytes.clone(), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url.clone(), Some(total as u64));
        let client = reqwest::Client::new();
        let session_id = "extent-cap-admission";
        seed_startup_range(&store, &client, session_id, &resource).await;
        let mut manifest = store
            .range_manifest_snapshot(session_id, &resource)
            .expect("startup manifest should validate")
            .expect("startup manifest should exist");
        let data_path = temp
            .path()
            .join(".tvos-net-player/hls/extent-cap-admission/video-resource.range.data");
        let mut data = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&data_path)
            .expect("range data should open");
        data.seek(SeekFrom::Start(0))
            .expect("data file should seek to its start");
        data.write_all(&bytes)
            .expect("fixture data should be written");
        data.seek(SeekFrom::Start(RANGE_STARTUP_CHUNK_BYTES))
            .expect("uncovered admission byte should seek");
        data.write_all(&[0xA5]).expect("sentinel should be written");
        data.sync_all().expect("fixture data should flush");

        for index in 0..(RANGE_MAX_CHUNKS as usize - manifest.extents.len()) {
            let start = RANGE_STARTUP_CHUNK_BYTES as usize + 2 + index * 2;
            manifest.extents.push(PersistedRangeExtent {
                start: start as u64,
                end: start as u64 + 1,
                sha256: format!("{:x}", Sha256::digest(&bytes[start..start + 1])),
            });
        }
        manifest.generation += 1;
        manifest.durable_bytes = RANGE_STARTUP_CHUNK_BYTES + (RANGE_MAX_CHUNKS - 1);
        let manifest_path = temp
            .path()
            .join(".tvos-net-player/hls/extent-cap-admission/video-resource.range.json");
        std::fs::write(
            manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("fixture manifest should serialize"),
        )
        .expect("fixture checkpoint should be written");
        let expected = store
            .range_manifest_snapshot(session_id, &resource)
            .expect("extent-cap manifest should validate")
            .expect("extent-cap manifest should exist");
        assert_eq!(RANGE_MAX_CHUNKS as usize, expected.extents.len());

        let range = RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + 1;
        let error = store
            .commit_range_extent(
                session_id,
                &resource,
                &expected,
                range,
                &bytes[RANGE_STARTUP_CHUNK_BYTES as usize..RANGE_STARTUP_CHUNK_BYTES as usize + 1],
                &url,
                None,
                None,
            )
            .await
            .expect_err("extent capacity should reject admission before writing");
        assert!(matches!(error, HlsRangeError::QuotaExceeded));
        let on_disk = std::fs::read(data_path).expect("range data should remain readable");
        assert_eq!(0xA5_u8, on_disk[RANGE_STARTUP_CHUNK_BYTES as usize]);
        assert_eq!(
            RANGE_STARTUP_CHUNK_BYTES + (RANGE_MAX_CHUNKS - 1),
            store
                .range_durable_bytes(session_id, &resource)
                .expect("durable checkpoint should remain readable")
        );
        stop_server(server).await;
    })
    .await
    .expect("extent-cap test should finish within its deadline");
}

#[tokio::test]
async fn range_manifest_admission_keeps_origin_and_byte_caps() {
    timeout(TEST_DEADLINE, async {
        let total = RANGE_STARTUP_CHUNK_BYTES as usize;
        let bytes = patterned_bytes(total);
        let (url, _state, server) = start_range_server(bytes, None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url, Some(total as u64));
        let client = reqwest::Client::new();
        seed_startup_range(&store, &client, "manifest-caps", &resource).await;
        let mut manifest = store
            .range_manifest_snapshot("manifest-caps", &resource)
            .expect("manifest should validate")
            .expect("manifest should exist");

        manifest.validated_origins = (0..65)
            .map(|index| PersistedRangeOrigin {
                origin: format!("https://origin-{index}.example:443"),
                prefix_sha256: "0".repeat(64),
                strong_etag: None,
            })
            .collect();
        assert!(matches!(
            ensure_range_manifest_capacity(&manifest),
            Err(HlsRangeError::QuotaExceeded)
        ));

        manifest.validated_origins = (0..64)
            .map(|index| PersistedRangeOrigin {
                origin: format!("https://{}.example:443", "x".repeat(40_000 + index)),
                prefix_sha256: "0".repeat(64),
                strong_etag: None,
            })
            .collect();
        assert!(matches!(
            ensure_range_manifest_capacity(&manifest),
            Err(HlsRangeError::QuotaExceeded)
        ));
        stop_server(server).await;
    })
    .await
    .expect("manifest cap test should finish within its deadline");
}

#[tokio::test]
async fn concurrent_distinct_extents_preserve_both_checkpoints() {
    timeout(TEST_DEADLINE, async {
        let total = (RANGE_STARTUP_CHUNK_BYTES * 3) as usize;
        let bytes = patterned_bytes(total);
        let (url, state, server) = start_range_server(bytes.clone(), None).await;
        let temp = tempfile::tempdir().expect("cache root should be created");
        let store = test_store(temp.path());
        let resource = resource(url, Some(total as u64));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("test client should build");

        store
            .ensure_resource_range(
                &client,
                "concurrent-extents",
                &resource,
                0..1,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("startup extent should be checkpointed");
        let left = RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES * 2;
        let right = RANGE_STARTUP_CHUNK_BYTES * 2..RANGE_STARTUP_CHUNK_BYTES * 3;
        let (left_result, right_result) = tokio::join!(
            store.ensure_resource_range(
                &client,
                "concurrent-extents",
                &resource,
                left.clone(),
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            ),
            store.ensure_resource_range(
                &client,
                "concurrent-extents",
                &resource,
                right.clone(),
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            ),
        );
        left_result.expect("first concurrent extent should become ready");
        right_result.expect("second concurrent extent should become ready");

        for range in [left, right] {
            assert_eq!(
                bytes[range.start as usize..range.end as usize],
                store
                    .read_resource_range("concurrent-extents", &resource, range)
                    .await
                    .expect("durable extent should read")
                    .expect("both concurrent extents should remain checkpointed")
            );
        }
        let counts = state
            .requests
            .lock()
            .expect("request list should not poison")
            .clone();
        assert_eq!(
            1,
            counts
                .iter()
                .filter(|range| **range == (0..RANGE_STARTUP_CHUNK_BYTES as usize))
                .count(),
            "startup extent should be fetched once"
        );
        let mut post_startup_ranges: Vec<_> = counts
            .iter()
            .filter(|range| range.start >= RANGE_STARTUP_CHUNK_BYTES as usize)
            .cloned()
            .collect();
        post_startup_ranges.sort_by_key(|range| range.start);
        let expected_post_startup_ranges: Vec<_> = (0..16)
            .map(|index| {
                let start = RANGE_STARTUP_CHUNK_BYTES as usize
                    + index * RANGE_FOREGROUND_CHUNK_BYTES as usize;
                start..start + RANGE_FOREGROUND_CHUNK_BYTES as usize
            })
            .collect();
        assert_eq!(expected_post_startup_ranges, post_startup_ranges);
        assert_eq!(
            (total as u64),
            store
                .range_durable_bytes("concurrent-extents", &resource)
                .expect("all checkpoints should be accounted")
        );
        stop_server(server).await;
    })
    .await
    .expect("concurrent extent test should finish within its deadline");
}

#[test]
fn shared_exact_range_foreground_subscriber_promotes_background_flight() {
    let cache = Arc::new(HlsRangeCache::new(1, 0));
    let key = RangeChunkKey {
        resource: RangeResourceKey {
            session_id: "promoted-flight".to_owned(),
            resource_id: "video-resource".to_owned(),
            representation: "representation".to_owned(),
        },
        total_length: 1024,
        range: 512..1024,
    };
    let background = cache
        .claim_chunk(key.clone(), HlsRangePriority::Background)
        .expect("background flight should start");
    assert!(background.is_owner());
    let _activity = cache
        .enter("promoted-flight", HlsRangePriority::Foreground)
        .expect("foreground activity should start");
    let foreground = cache
        .claim_chunk(key.clone(), HlsRangePriority::Foreground)
        .expect("same-range foreground should join the flight");
    assert!(!foreground.is_owner());
    assert_eq!(1, cache.active_flight_count(&key.resource));
    cache
        .check_priority_control(&key, HlsRangePriority::Background, &|| {
            HlsCacheFillControl::Preempt
        })
        .expect("exact foreground subscriber should promote the shared background flight");
}

#[tokio::test]
async fn dropping_foreground_flight_owner_wakes_waiter() {
    timeout(TEST_DEADLINE, async {
        let cache = Arc::new(HlsRangeCache::new(1, 4096));
        let key = RangeChunkKey {
            resource: RangeResourceKey {
                session_id: "dedupe-waiter".to_owned(),
                resource_id: "video-resource".to_owned(),
                representation: "representation".to_owned(),
            },
            total_length: 64,
            range: 0..64,
        };
        let owner = cache
            .claim_chunk(key.clone(), HlsRangePriority::Foreground)
            .expect("first claimant should own the flight");
        assert!(owner.is_owner());
        let waiter = cache
            .claim_chunk(key, HlsRangePriority::Foreground)
            .expect("second claimant should join the flight");
        assert!(!waiter.is_owner());

        drop(owner);
        timeout(Duration::from_secs(1), waiter.notified())
            .await
            .expect("owner drop should notify the waiter");
        assert!(cache.lock_state().flights.is_empty());
    })
    .await
    .expect("foreground dedupe test should finish within its deadline");
}

#[tokio::test]
async fn quota_release_allows_new_reservation_and_removal_blocks_late_activity() {
    timeout(TEST_DEADLINE, async {
        let cache = Arc::new(HlsRangeCache::new(1, 1024));
        let first_key = RangeResourceKey {
            session_id: "quota-session".to_owned(),
            resource_id: "first".to_owned(),
            representation: "rep".to_owned(),
        };
        let second_key = RangeResourceKey {
            session_id: "quota-session".to_owned(),
            resource_id: "second".to_owned(),
            representation: "rep".to_owned(),
        };
        let lease = cache
            .reserve(first_key, 800, 0, 0)
            .expect("first quota reservation should fit");
        assert!(matches!(
            cache.reserve(second_key.clone(), 300, 0, 0),
            Err(HlsRangeError::QuotaExceeded)
        ));
        drop(lease);
        let released = cache
            .reserve(second_key, 300, 0, 0)
            .expect("released reservation should return quota");
        assert_eq!((0, 1), cache.range_activity_counts());
        drop(released);
        assert_eq!((0, 0), cache.range_activity_counts());

        let active = cache
            .enter("removing-session", HlsRangePriority::Foreground)
            .expect("session activity should start");
        let removal_cache = Arc::clone(&cache);
        let removal = tokio::spawn(async move {
            removal_cache
                .begin_session_removal("removing-session")
                .await
        });
        timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    cache.enter("removing-session", HlsRangePriority::Background),
                    Err(HlsRangeError::SessionRemoving)
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("removal should block new late activity");
        assert!(!removal.is_finished(), "removal must wait for active work");
        drop(active);
        let guard = timeout(Duration::from_secs(1), removal)
            .await
            .expect("removal should finish after active work drains")
            .expect("removal task should not panic")
            .expect("session removal guard should be returned");
        guard.commit();
        assert!(matches!(
            cache.enter("removing-session", HlsRangePriority::Background),
            Err(HlsRangeError::SessionRemoving)
        ));
    })
    .await
    .expect("quota and removal test should finish within its deadline");
}

#[tokio::test]
async fn global_drain_waits_for_activity_and_leaves_no_flights_or_reservations() {
    timeout(TEST_DEADLINE, async {
        let cache = Arc::new(HlsRangeCache::new(1, 4096));
        let active = cache
            .enter("drain-session", HlsRangePriority::Background)
            .expect("activity should start before drain");
        let reservation = cache
            .reserve(
                RangeResourceKey {
                    session_id: "drain-session".to_owned(),
                    resource_id: "video-resource".to_owned(),
                    representation: "representation".to_owned(),
                },
                128,
                0,
                0,
            )
            .expect("quota reservation should start before drain");
        let flight_key = RangeChunkKey {
            resource: RangeResourceKey {
                session_id: "drain-session".to_owned(),
                resource_id: "video-resource".to_owned(),
                representation: "representation".to_owned(),
            },
            total_length: 128,
            range: 0..128,
        };
        let flight = cache
            .claim_chunk(flight_key, HlsRangePriority::Background)
            .expect("flight should start before drain");
        let drain_cache = Arc::clone(&cache);
        let drain = tokio::spawn(async move { drain_cache.drain().await });

        timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    cache.enter("late-session", HlsRangePriority::Background),
                    Err(HlsRangeError::SessionRemoving)
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("global drain should reject new activity");
        assert!(!drain.is_finished(), "drain must wait for active work");

        drop(flight);
        drop(reservation);
        drop(active);
        timeout(Duration::from_secs(1), drain)
            .await
            .expect("global drain should finish after activity drops")
            .expect("drain task should not panic");
        assert_eq!((0, 0), cache.range_activity_counts());
        let state = cache.lock_state();
        assert!(
            state.flights.is_empty(),
            "drain must leave no flight entries"
        );
        assert!(
            state.reservations.is_empty(),
            "drain must leave no quota reservations"
        );
    })
    .await
    .expect("global drain test should finish within its deadline");
}
