use super::{
    AcceptanceReport, AppState, CacheServerOptions, HlsCacheStore, HlsPlaybackSession, NativeCase,
    OriginMedia, OriginState, PHASE_BASELINE, PlaybackPolicy, RejectNetworkPlanner,
    RepresentationPacer, VariantSpec, fixture_strong_etag, local_only_http_client, make_variant,
    mp4_initialization_length, origin_get, origin_response, publish_playable_session,
    require_high_playback_sidx_maps,
};
use axum::{
    Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, HeaderValue, Response, StatusCode, header::ETAG},
    routing::get,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicU8},
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, task::JoinHandle};

const FIXTURE_MDAT_PAYLOAD_BYTES: usize = 1024 * 1024 + 256;
const FIXTURE_SESSION_CONTENT_ID: &str = "native-media-acceptance";

#[derive(Clone, Copy, Debug)]
enum ValidatorMode {
    Strong,
    Missing,
    Weak,
}

struct FixtureHarness {
    state: AppState,
    session: HlsPlaybackSession,
    generation: u64,
    origin_base: String,
    media_base: String,
    expected_etags: [String; 2],
    client: reqwest::Client,
    tasks: Vec<JoinHandle<()>>,
    _temp: tempfile::TempDir,
}

impl Drop for FixtureHarness {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl FixtureHarness {
    async fn new(mode: ValidatorMode) -> Self {
        let temp = tempfile::tempdir().expect("fixture root should be created");
        let root = temp
            .path()
            .canonicalize()
            .expect("fixture root should canonicalize");
        let library_root = root.join("library");
        tokio::fs::create_dir_all(&library_root)
            .await
            .expect("fixture library root should be created");
        tokio::fs::create_dir_all(root.join(".state"))
            .await
            .expect("fixture task-state root should be created");

        let origin_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture origin should bind to loopback");
        let origin_address = origin_listener
            .local_addr()
            .expect("fixture origin address should be available");
        let media_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("production media router should bind to loopback");
        let media_address = media_listener
            .local_addr()
            .expect("production media router address should be available");
        let origin_base = format!("http://{origin_address}");
        let media_base = format!("http://{media_address}");

        let high_path = root.join("high.mp4");
        let audio_path = root.join("audio.mp4");
        let high_bytes = indexed_mp4(FIXTURE_MDAT_PAYLOAD_BYTES, 0x31);
        let audio_bytes = indexed_mp4(FIXTURE_MDAT_PAYLOAD_BYTES, 0x72);
        tokio::fs::write(&high_path, &high_bytes)
            .await
            .expect("high-video fixture should be written");
        tokio::fs::write(&audio_path, &audio_bytes)
            .await
            .expect("required-audio fixture should be written");

        let high_media = origin_media("high.mp4", high_path, 1, "video/mp4", &high_bytes);
        let audio_media = origin_media("audio.mp4", audio_path, 3, "audio/mp4", &audio_bytes);
        let expected_etags = [
            high_media.strong_etag.clone(),
            audio_media.strong_etag.clone(),
        ];
        let mut media = HashMap::new();
        media.insert("high.mp4".to_owned(), high_media);
        media.insert("audio.mp4".to_owned(), audio_media);
        let report = Arc::new(Mutex::new(AcceptanceReport::new(
            NativeCase::BufferedContinuity,
        )));
        let origin = Arc::new(OriginState {
            media,
            case: NativeCase::BufferedContinuity,
            phase: Arc::new(AtomicU8::new(PHASE_BASELINE)),
            epoch: Arc::new(Mutex::new(Instant::now())),
            report,
            pacers: std::array::from_fn(|_| RepresentationPacer::default()),
        });
        let origin_route = match mode {
            ValidatorMode::Strong => get(origin_get),
            ValidatorMode::Missing => get(origin_get_without_etag),
            ValidatorMode::Weak => get(origin_get_with_weak_etag),
        };
        let origin_router = Router::new()
            .route("/{file_name}", origin_route)
            .with_state(Arc::clone(&origin));

        let options = CacheServerOptions {
            root_path: library_root,
            task_state_path: root.join(".state").join("tasks.json"),
            media_listen_url: media_base.clone(),
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };
        let mut state = AppState::new_with_playback_planner_and_hls_cache(
            options,
            Arc::new(RejectNetworkPlanner),
            HlsCacheStore::new(root.join("cache")),
        );
        state.hls_upstream_client =
            local_only_http_client().expect("local-only upstream client should build");

        let high_url = format!("{origin_base}/high.mp4");
        let audio_url = format!("{origin_base}/audio.mp4");
        let selected = make_variant(VariantSpec {
            id: "high",
            width: 640,
            height: 360,
            bandwidth: 1_000_000,
            request_bandwidth: 1_000_000,
            duration: 20,
            video_url: &high_url,
            audio_url: &audio_url,
            video_codec: "avc1.64001f",
            level: None,
        });
        let source_variants = vec![selected.clone()];
        let abr = super::BilibiliPlaybackAbrMetadata { groups: Vec::new() };
        let task_creation = state
            .tasks
            .create_bilibili_playback_task(FIXTURE_SESSION_CONTENT_ID, None, None)
            .expect("credential-free fixture playback task should be created");
        let task_id = task_creation.task.id;
        let session = HlsPlaybackSession::from_playback_entry_with_policy(
            &task_id,
            "Deterministic high-preparation SIDX fixture",
            &selected,
            &abr,
            &source_variants,
            PlaybackPolicy::default(),
        )
        .expect("fixture HLS session should be constructed");
        let master_url = format!("{media_base}/hls/{task_id}/master.m3u8");
        publish_playable_session(&state, &session, &task_id, &master_url)
            .expect("fixture playback session should be published locally");
        let generation = state
            .hls_sessions
            .get_with_generation(&session.id)
            .expect("fixture session should have a current generation")
            .generation;

        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(8))
            .build()
            .expect("bounded loopback client should build");
        let media_router = crate::media_router(state.clone());
        let origin_task = tokio::spawn(async move {
            axum::serve(origin_listener, origin_router)
                .await
                .expect("fixture origin should serve");
        });
        let media_task = tokio::spawn(async move {
            axum::serve(media_listener, media_router)
                .await
                .expect("production media router should serve");
        });

        Self {
            state,
            session,
            generation,
            origin_base,
            media_base,
            expected_etags,
            client,
            tasks: vec![origin_task, media_task],
            _temp: temp,
        }
    }

    async fn assert_origin_validator(&self, mode: ValidatorMode) {
        for (resource, expected_etag) in [
            ("high.mp4", self.expected_etags[0].as_str()),
            ("audio.mp4", self.expected_etags[1].as_str()),
        ] {
            let response = self
                .client
                .get(format!("{}/{resource}", self.origin_base))
                .header("range", "bytes=0-0")
                .send()
                .await
                .expect("bounded fixture-origin validator probe should complete");
            assert_eq!(
                StatusCode::PARTIAL_CONTENT,
                response.status(),
                "origin validator probe failed for {resource}"
            );
            let actual_etag = response.headers().get(ETAG).map(|value| {
                value
                    .to_str()
                    .expect("ETag should be valid text")
                    .to_owned()
            });
            match mode {
                ValidatorMode::Strong => assert_eq!(Some(expected_etag), actual_etag.as_deref()),
                ValidatorMode::Missing => assert!(actual_etag.is_none()),
                ValidatorMode::Weak => assert!(
                    actual_etag
                        .as_deref()
                        .is_some_and(|etag| etag.starts_with("W/"))
                ),
            }
            assert_eq!(
                1,
                response
                    .bytes()
                    .await
                    .expect("validator probe body should complete")
                    .len()
            );
        }
    }

    async fn load_playlist(&self, playlist: &str) {
        let response = self
            .client
            .get(format!(
                "{}/hls/{}/segments/{playlist}",
                self.media_base, self.session.id
            ))
            .send()
            .await
            .expect("production media playlist request should complete");
        let status = response.status();
        let body = response
            .text()
            .await
            .expect("production playlist should be readable");
        assert_eq!(StatusCode::OK, status, "{playlist} body={body:?}");
        assert_eq!(
            2,
            body.matches("#EXTINF:").count(),
            "{playlist} did not contain both parsed SIDX segments: {body:?}"
        );
        assert_eq!(
            2,
            body.matches("#EXT-X-BYTERANGE:").count(),
            "{playlist} did not contain both SIDX byte ranges: {body:?}"
        );
    }

    fn assert_maps_absent_and_checker_rejects(&self) {
        let audio = self
            .session
            .variant
            .audio
            .as_ref()
            .expect("fixture target variant should have required audio");
        assert!(
            self.state
                .hls_cache
                .playback_sidx_map(
                    &self.session.id,
                    self.generation,
                    &self.session.variant.video
                )
                .is_none(),
            "high-video map must not register without a strong validator"
        );
        assert!(
            self.state
                .hls_cache
                .playback_sidx_map(&self.session.id, self.generation, audio)
                .is_none(),
            "required-audio map must not register without a strong validator"
        );
        let error = require_high_playback_sidx_maps(&self.state, &self.session, self.generation)
            .expect_err("checker must reject the absent high-video and required-audio maps");
        assert!(
            error.contains("high video playback SIDX map was not registered"),
            "checker rejected for an unexpected reason: {error}"
        );
    }
}

fn origin_media(
    resource_name: &str,
    path: PathBuf,
    representation_code: u8,
    content_type: &'static str,
    bytes: &[u8],
) -> OriginMedia {
    OriginMedia {
        strong_etag: fixture_strong_etag(resource_name, &path)
            .expect("synthetic origin resource should have a strong ETag"),
        representation_code,
        initialization_length: mp4_initialization_length(bytes)
            .expect("synthetic MP4 initialization should parse"),
        path,
        fragments: Vec::new(),
        content_type,
    }
}

fn indexed_mp4(payload_bytes: usize, pattern: u8) -> Vec<u8> {
    let first_payload_bytes = payload_bytes / 2;
    let first_fragment = mp4_box(*b"mdat", &vec![pattern; first_payload_bytes]);
    let second_fragment = mp4_box(
        *b"mdat",
        &vec![pattern.wrapping_add(1); payload_bytes - first_payload_bytes],
    );

    let mut sidx_payload = Vec::new();
    sidx_payload.extend([0_u8; 4]);
    sidx_payload.extend(1_u32.to_be_bytes());
    sidx_payload.extend(1_000_u32.to_be_bytes());
    sidx_payload.extend(0_u32.to_be_bytes());
    sidx_payload.extend(0_u32.to_be_bytes());
    sidx_payload.extend([0_u8; 2]);
    sidx_payload.extend(2_u16.to_be_bytes());
    for fragment in [&first_fragment, &second_fragment] {
        sidx_payload.extend(
            u32::try_from(fragment.len())
                .expect("fixture SIDX reference should fit in 32 bits")
                .to_be_bytes(),
        );
        sidx_payload.extend(5_000_u32.to_be_bytes());
        sidx_payload.extend(0x9000_0000_u32.to_be_bytes());
    }

    let mut bytes = Vec::new();
    bytes.extend(mp4_box(*b"ftyp", b"isom"));
    bytes.extend(mp4_box(*b"moov", b"metadata"));
    bytes.extend(mp4_box(*b"sidx", &sidx_payload));
    bytes.extend(first_fragment);
    bytes.extend(second_fragment);
    bytes
}

fn mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = u32::try_from(8 + payload.len()).expect("fixture MP4 box should fit in 32 bits");
    let mut bytes = Vec::with_capacity(size as usize);
    bytes.extend(size.to_be_bytes());
    bytes.extend(kind);
    bytes.extend(payload);
    bytes
}

async fn origin_get_without_etag(
    State(origin): State<Arc<OriginState>>,
    AxumPath(file_name): AxumPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let mut response = origin_response(origin, file_name, headers, false).await;
    response.headers_mut().remove(ETAG);
    response
}

async fn origin_get_with_weak_etag(
    State(origin): State<Arc<OriginState>>,
    AxumPath(file_name): AxumPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let mut response = origin_response(origin, file_name, headers, false).await;
    response.headers_mut().insert(
        ETAG,
        HeaderValue::from_static("W/\"deterministic-fixture-v1\""),
    );
    response
}

#[tokio::test]
async fn production_playlist_route_registers_high_video_and_required_audio_maps_with_strong_etags()
{
    let harness = FixtureHarness::new(ValidatorMode::Strong).await;
    harness.assert_origin_validator(ValidatorMode::Strong).await;
    harness.load_playlist("video.m3u8").await;
    harness.load_playlist("audio.m3u8").await;

    let current = harness
        .state
        .hls_sessions
        .get_with_generation(&harness.session.id)
        .expect("target HLS session should remain registered");
    assert_eq!(harness.generation, current.generation);
    assert_eq!(harness.session.variant.id, current.session.variant.id);
    let audio = harness
        .session
        .variant
        .audio
        .as_ref()
        .expect("target high variant should retain required audio");
    assert!(
        harness
            .state
            .hls_cache
            .playback_sidx_map(
                &harness.session.id,
                harness.generation,
                &harness.session.variant.video
            )
            .is_some(),
        "actual high-video playlist should register its current-generation SIDX map"
    );
    assert!(
        harness
            .state
            .hls_cache
            .playback_sidx_map(&harness.session.id, harness.generation, audio)
            .is_some(),
        "actual required-audio playlist should register its current-generation SIDX map"
    );
    require_high_playback_sidx_maps(&harness.state, &harness.session, harness.generation)
        .expect("required high-video and audio maps should satisfy the acceptance checker");
}

#[tokio::test]
async fn production_playlist_route_does_not_register_maps_without_etags() {
    let harness = FixtureHarness::new(ValidatorMode::Missing).await;
    harness
        .assert_origin_validator(ValidatorMode::Missing)
        .await;
    harness.load_playlist("video.m3u8").await;
    harness.load_playlist("audio.m3u8").await;
    harness.assert_maps_absent_and_checker_rejects();
}

#[tokio::test]
async fn production_playlist_route_does_not_register_maps_with_weak_etags() {
    let harness = FixtureHarness::new(ValidatorMode::Weak).await;
    harness.assert_origin_validator(ValidatorMode::Weak).await;
    harness.load_playlist("video.m3u8").await;
    harness.load_playlist("audio.m3u8").await;
    harness.assert_maps_absent_and_checker_rejects();
}
