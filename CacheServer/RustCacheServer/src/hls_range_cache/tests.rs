use std::{
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
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};

use crate::{
    bbdown_adapter::{BilibiliMediaCacheKey, BilibiliMediaRequest, BilibiliMediaRequestKind},
    hls::HlsMediaResource,
    hls_cache::{HlsCacheFillControl, HlsCacheStore},
};

use super::*;

const TEST_DEADLINE: Duration = Duration::from_secs(8);

struct MockRangeState {
    bytes: Arc<Vec<u8>>,
    requests: Mutex<Vec<Range<usize>>>,
    fail_once: Option<Range<usize>>,
    failed: AtomicBool,
}

async fn start_range_server(
    bytes: Vec<u8>,
    fail_once: Option<Range<usize>>,
) -> (String, Arc<MockRangeState>, JoinHandle<()>) {
    let state = Arc::new(MockRangeState {
        bytes: Arc::new(bytes),
        requests: Mutex::new(Vec::new()),
        fail_once,
        failed: AtomicBool::new(false),
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
        let left = RANGE_STARTUP_CHUNK_BYTES..RANGE_STARTUP_CHUNK_BYTES + 1;
        let right = RANGE_STARTUP_CHUNK_BYTES * 2..RANGE_STARTUP_CHUNK_BYTES * 2 + 1;
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
        assert!(counts.contains(&(0..RANGE_STARTUP_CHUNK_BYTES as usize)));
        assert!(counts.contains(
            &(RANGE_STARTUP_CHUNK_BYTES as usize..(RANGE_STARTUP_CHUNK_BYTES * 2) as usize)
        ));
        assert!(counts.contains(&((RANGE_STARTUP_CHUNK_BYTES * 2) as usize..total)));
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
