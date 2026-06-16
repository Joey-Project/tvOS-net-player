use std::{net::TcpListener, path::Path, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
#[cfg(target_os = "macos")]
use reqwest::StatusCode;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tonic::{Code, Request};
use tvos_net_player_cache_server::{
    AppState,
    config::CacheServerOptions,
    generated::tvos_net_player::v1::{
        CancelTaskRequest, CheckHealthRequest, CreateBilibiliTaskRequest, DeleteLibraryItemRequest,
        GetHlsCacheStatusRequest, GetLibraryItemRequest, GetPlaybackSourceRequest,
        GetServerInfoRequest, GetTaskRequest, LibrarySource, ListCacheRootsRequest,
        ListLibraryItemsRequest, RescanLibraryRequest, ServerCapability, TaskState,
        WatchTasksRequest, cache_service_client::CacheServiceClient,
        library_service_client::LibraryServiceClient, server_service_client::ServerServiceClient,
        task_service_client::TaskServiceClient,
    },
    run_grpc_server, run_media_server,
};

#[cfg(target_os = "macos")]
use tvos_net_player_cache_server::generated::tvos_net_player::v1::PlaybackProtocol;

#[tokio::test]
async fn serves_library_control_plane_and_http_range_media() {
    let server = TestServer::start().await;
    let channel = server.channel().await;

    let mut server_client = ServerServiceClient::new(channel.clone());
    let info = server_client
        .get_server_info(GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!("Test Cache", info.name);
    assert!(
        info.capabilities
            .contains(&(ServerCapability::BilibiliTasks as i32))
    );
    assert!(info.capabilities.contains(&(ServerCapability::Hls as i32)));
    assert!(
        info.capabilities
            .contains(&(ServerCapability::LibraryItemDelete as i32))
    );
    #[cfg(target_os = "macos")]
    assert!(
        info.capabilities
            .contains(&(ServerCapability::HttpRange as i32))
    );
    #[cfg(not(target_os = "macos"))]
    assert!(
        !info
            .capabilities
            .contains(&(ServerCapability::HttpRange as i32))
    );

    let health = server_client
        .check_health(CheckHealthRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        tvos_net_player_cache_server::generated::tvos_net_player::v1::HealthState::Serving as i32,
        health.state
    );

    let mut library_client = LibraryServiceClient::new(channel.clone());
    let library = library_client
        .list_library_items(ListLibraryItemsRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(1, library.items.len());
    let item = &library.items[0];
    assert_eq!("Sample Clip", item.title);
    assert_eq!("Movies/Sample Clip.mp4", item.subtitle);
    assert_eq!(LibrarySource::LocalCache as i32, item.source);

    #[cfg(target_os = "macos")]
    {
        assert_eq!(1, item.variants.len());
        assert_eq!(PlaybackProtocol::HttpFile as i32, item.variants[0].protocol);

        let playback = library_client
            .get_playback_source(GetPlaybackSourceRequest {
                item_id: item.id.clone(),
                variant_id: "original".to_owned(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(PlaybackProtocol::HttpFile as i32, playback.protocol);

        let http = reqwest::Client::new();
        let range_response = http
            .get(&playback.uri)
            .header(reqwest::header::RANGE, "bytes=4-7")
            .send()
            .await
            .unwrap();
        assert_eq!(StatusCode::PARTIAL_CONTENT, range_response.status());
        assert_eq!("bytes", range_response.headers()["accept-ranges"]);
        assert_eq!("bytes 4-7/16", range_response.headers()["content-range"]);
        assert_eq!("4567", range_response.text().await.unwrap());

        let head_response = http.head(&playback.uri).send().await.unwrap();
        assert_eq!(StatusCode::OK, head_response.status());
        assert_eq!("16", head_response.headers()["content-length"]);
        assert_eq!(0, head_response.bytes().await.unwrap().len());

        let wrong_port_response = http
            .get(format!(
                "{}/media/{}/original",
                server.grpc_url,
                urlencoding::encode(&item.id)
            ))
            .send()
            .await;
        assert!(
            wrong_port_response
                .map(|response| !response.status().is_success())
                .unwrap_or(true),
            "media must not be served from the gRPC listener"
        );
    }

    #[cfg(not(target_os = "macos"))]
    {
        assert!(item.variants.is_empty());
        let error = library_client
            .get_playback_source(GetPlaybackSourceRequest {
                item_id: item.id.clone(),
                variant_id: "original".to_owned(),
            })
            .await
            .unwrap_err();
        assert_eq!(Code::FailedPrecondition, error.code());
    }
}

#[tokio::test]
async fn rejects_path_escape_and_symlink_media() {
    let server = TestServer::start().await;
    let channel = server.channel().await;
    let mut library_client = LibraryServiceClient::new(channel);

    let traversal_id = item_id("../outside.mp4");
    let traversal_error = library_client
        .get_library_item(GetLibraryItemRequest { id: traversal_id })
        .await
        .unwrap_err();
    assert_eq!(Code::NotFound, traversal_error.code());

    let linked_id = item_id("Movies/Linked Outside.mp4");
    #[cfg(target_os = "macos")]
    let linked_error = library_client
        .get_playback_source(GetPlaybackSourceRequest {
            item_id: linked_id,
            variant_id: "original".to_owned(),
        })
        .await
        .unwrap_err();
    #[cfg(not(target_os = "macos"))]
    let linked_error = library_client
        .get_playback_source(GetPlaybackSourceRequest {
            item_id: linked_id,
            variant_id: "original".to_owned(),
        })
        .await
        .unwrap_err();
    #[cfg(target_os = "macos")]
    assert_eq!(Code::NotFound, linked_error.code());
    #[cfg(not(target_os = "macos"))]
    assert_eq!(Code::FailedPrecondition, linked_error.code());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn derives_playback_host_from_grpc_local_addr_for_wildcard_media_listener() {
    let server = TestServer::start_with_media_listen_host("0.0.0.0").await;
    let channel = server.channel().await;
    let mut library_client = LibraryServiceClient::new(channel);
    let library = library_client
        .list_library_items(ListLibraryItemsRequest::default())
        .await
        .unwrap()
        .into_inner();
    let item = &library.items[0];

    let playback = library_client
        .get_playback_source(GetPlaybackSourceRequest {
            item_id: item.id.clone(),
            variant_id: "original".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();

    assert!(
        playback
            .uri
            .starts_with(&format!("{}/media/", server.media_url)),
        "playback uri should use the gRPC socket local address: {}",
        playback.uri
    );
}

#[tokio::test]
async fn returns_not_found_for_missing_hls_playback_session() {
    let server = TestServer::start().await;
    let http = reqwest::Client::new();

    let playlist_response = http
        .get(format!("{}/hls/session-1/master.m3u8", server.media_url))
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::NOT_FOUND, playlist_response.status());

    let segment_response = http
        .head(format!(
            "{}/hls/session-1/segments/segment-1.ts",
            server.media_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::NOT_FOUND, segment_response.status());
}

#[tokio::test]
async fn supports_cache_roots_rescan_and_bilibili_task_lifecycle() {
    let server = TestServer::start().await;
    let channel = server.channel().await;

    let mut cache_client = CacheServiceClient::new(channel.clone());
    let roots = cache_client
        .list_cache_roots(ListCacheRootsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(1, roots.roots.len());
    assert_eq!("default", roots.roots[0].id);
    assert!(roots.roots[0].writable);
    let hls_status = cache_client
        .get_hls_cache_status(GetHlsCacheStatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(hls_status.eviction_enabled);
    assert_eq!(50 * 1024 * 1024 * 1024, hls_status.max_bytes);
    assert_eq!(90, hls_status.high_watermark_percent);
    assert_eq!(80, hls_status.low_watermark_percent);
    assert_eq!(0, hls_status.used_bytes);
    assert_eq!(0, hls_status.completed_session_count);

    let mut library_client = LibraryServiceClient::new(channel.clone());
    let library = library_client
        .list_library_items(ListLibraryItemsRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(1, library.items.len());
    let deleted = cache_client
        .delete_library_item(DeleteLibraryItemRequest {
            id: library.items[0].id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(deleted.deleted);
    assert!(
        !server
            ._temp_root
            .path()
            .join("Movies")
            .join("Sample Clip.mp4")
            .exists()
    );
    let repeated_delete = cache_client
        .delete_library_item(DeleteLibraryItemRequest {
            id: library.items[0].id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!repeated_delete.deleted);

    let rescan = library_client
        .rescan_library(RescanLibraryRequest {
            cache_root_ids: vec!["default".to_owned()],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(0, rescan.discovered_item_count);
    let missing_root = library_client
        .rescan_library(RescanLibraryRequest {
            cache_root_ids: vec!["missing".to_owned()],
        })
        .await
        .unwrap_err();
    assert_eq!(Code::NotFound, missing_root.code());

    let mut task_client = TaskServiceClient::new(channel.clone());
    let created = task_client
        .create_bilibili_task(CreateBilibiliTaskRequest {
            url_or_id: "  BV1task  ".to_owned(),
            options: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(created.id.starts_with("bilibili-"));
    assert_eq!(TaskState::Queued as i32, created.state);
    assert_eq!("BV1task", created.source);

    let duplicate = task_client
        .create_bilibili_task(CreateBilibiliTaskRequest {
            url_or_id: "BV1task".to_owned(),
            options: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(created.id, duplicate.id);

    let mut watch_stream = task_client
        .watch_tasks(Request::new(WatchTasksRequest {
            ids: vec![created.id.clone()],
        }))
        .await
        .unwrap()
        .into_inner();
    let snapshot = tokio::time::timeout(Duration::from_secs(2), watch_stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(created.id, snapshot.task.unwrap().id);

    let cancelled = task_client
        .cancel_task(CancelTaskRequest {
            id: created.id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(TaskState::Cancelled as i32, cancelled.state);

    let update = tokio::time::timeout(Duration::from_secs(2), watch_stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(TaskState::Cancelled as i32, update.task.unwrap().state);

    let cancelled_again = task_client
        .cancel_task(CancelTaskRequest {
            id: created.id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(created.id, cancelled_again.id);

    let requeued = task_client
        .create_bilibili_task(CreateBilibiliTaskRequest {
            url_or_id: "BV1task".to_owned(),
            options: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_ne!(created.id, requeued.id);

    let empty_source = task_client
        .create_bilibili_task(CreateBilibiliTaskRequest {
            url_or_id: " ".to_owned(),
            options: None,
        })
        .await
        .unwrap_err();
    assert_eq!(Code::InvalidArgument, empty_source.code());

    let missing_task = task_client
        .get_task(GetTaskRequest {
            id: "missing".to_owned(),
        })
        .await
        .unwrap_err();
    assert_eq!(Code::NotFound, missing_task.code());
}

#[tokio::test]
async fn streams_complete_large_task_snapshot() {
    let server = TestServer::start().await;
    let channel = server.channel().await;
    let mut task_client = TaskServiceClient::new(channel);

    let mut expected_ids = Vec::new();
    for index in 0..130 {
        let task = task_client
            .create_bilibili_task(CreateBilibiliTaskRequest {
                url_or_id: format!("BVlarge{index}"),
                options: None,
            })
            .await
            .unwrap()
            .into_inner();
        expected_ids.push(task.id);
    }

    let mut stream = task_client
        .watch_tasks(WatchTasksRequest { ids: Vec::new() })
        .await
        .unwrap()
        .into_inner();
    let mut actual_ids = Vec::new();
    for _ in 0..expected_ids.len() {
        let event = tokio::time::timeout(Duration::from_secs(2), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        actual_ids.push(event.task.unwrap().id);
    }

    expected_ids.sort();
    actual_ids.sort();
    assert_eq!(expected_ids, actual_ids);
}

struct TestServer {
    _temp_root: TempDir,
    _outside_root: TempDir,
    grpc_url: String,
    media_url: String,
    _grpc_task: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    _media_task: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_media_listen_host("127.0.0.1").await
    }

    async fn start_with_media_listen_host(media_listen_host: &str) -> Self {
        let temp_root = tempfile::tempdir().unwrap();
        let root_path = temp_root.path().canonicalize().unwrap();
        let outside_root = tempfile::tempdir().unwrap();
        let movie_dir = root_path.join("Movies");
        std::fs::create_dir_all(&movie_dir).unwrap();
        std::fs::write(movie_dir.join("Sample Clip.mp4"), b"0123456789abcdef").unwrap();

        let outside = outside_root.path().join("Outside.mp4");
        std::fs::write(&outside, b"outside-cache-root").unwrap();
        create_symlink(&outside, &movie_dir.join("Linked Outside.mp4"));

        let grpc_port = free_port();
        let media_port = free_port();
        let grpc_url = format!("http://127.0.0.1:{grpc_port}");
        let media_listen_url = format!("http://{media_listen_host}:{media_port}");
        let media_url = format!("http://127.0.0.1:{media_port}");
        let state = AppState::new(CacheServerOptions {
            server_name: "Test Cache".to_owned(),
            task_state_path: root_path.join(".state").join("tasks.json"),
            root_path,
            grpc_listen_url: grpc_url.clone(),
            media_listen_url,
            allow_library_item_delete: true,
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        });

        let grpc_task = tokio::spawn(run_grpc_server(
            state.options.grpc_listen_addr().unwrap(),
            state.clone(),
        ));
        let media_task = tokio::spawn(run_media_server(
            state.options.media_listen_addr().unwrap(),
            state,
        ));

        wait_for_grpc(&grpc_url).await;
        Self {
            _temp_root: temp_root,
            _outside_root: outside_root,
            grpc_url,
            media_url,
            _grpc_task: grpc_task,
            _media_task: media_task,
        }
    }

    async fn channel(&self) -> tonic::transport::Channel {
        tonic::transport::Channel::from_shared(self.grpc_url.clone())
            .unwrap()
            .connect()
            .await
            .unwrap()
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_for_grpc(grpc_url: &str) {
    for _ in 0..50 {
        if tonic::transport::Channel::from_shared(grpc_url.to_owned())
            .unwrap()
            .connect()
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    panic!("gRPC server did not start");
}

fn item_id(relative_path: &str) -> String {
    format!(
        "local.default.{}",
        URL_SAFE_NO_PAD.encode(relative_path.as_bytes())
    )
}

fn create_symlink(source: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, link).unwrap();

    #[cfg(windows)]
    std::os::windows::fs::symlink_file(source, link).unwrap();
}
