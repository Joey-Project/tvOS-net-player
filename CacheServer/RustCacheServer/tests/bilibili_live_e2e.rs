use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::FutureExt;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tonic::Request;
use tvos_net_player_cache_server::{
    AppState,
    config::CacheServerOptions,
    generated::tvos_net_player::v1::{
        BilibiliContentIdentity, BilibiliContentKind, BilibiliCredentialStatus,
        BilibiliPlaybackOptions, BilibiliPlaybackSpec, BilibiliResolutionCandidate,
        BilibiliResolutionPage, BilibiliResolutionSelection, BilibiliResolutionSelectionMode,
        BilibiliTaskResultItem, BilibiliVideoCodec, CacheResourceRef, CreateBilibiliTaskV2Request,
        GetBilibiliCredentialStatusRequest, GetPlaybackSourceRequest, GetTaskRequest,
        HlsCacheFillState, ListBilibiliResolutionCandidatesRequest, ListTaskResultsRequest,
        PageRequest, PlaybackProtocol, PlaybackSource, StartBilibiliResolutionRequest, Task,
        TaskArtifact, TaskResult, TaskState,
        create_bilibili_task_v2_request::Execution as BilibiliExecutionV2,
        library_service_client::LibraryServiceClient, server_service_client::ServerServiceClient,
        task_service_client::TaskServiceClient,
    },
    run_grpc_listener, run_media_listener,
};

const BILIBILI_RESOLUTION_PAGE_SIZE: u32 = 1;
const BILIBILI_TASK_RESULT_PAGE_SIZE: u32 = 1;
const LIVE_CASE_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(60);
const FULL_FILL_DEADLINE: Duration = Duration::from_secs(1_200);
const OFFLINE_READ_DURATION: Duration = Duration::from_secs(300);
const READY_FILE_LIMIT: usize = 4 * 1024;
const FULL_FILL_CACHE_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
const OFFLINE_MAX_VERIFIED_BYTES: u64 = FULL_FILL_CACHE_BUDGET_BYTES;
const OFFLINE_MAX_RESOURCES: usize = 4_096;
const SUSTAINED_HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const SUSTAINED_READ_LIMIT: u64 = 64 * 1024;
const SUSTAINED_PLAYLIST_LIMIT: usize = 1024 * 1024;
const SUSTAINED_FAILURE_BODY_LIMIT: usize = 4 * 1024;
const SUSTAINED_DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(1);
const SUSTAINED_MAX_CHILD_PLAYLISTS: usize = 32;
const BILIBILI_FAILURE_CLASS_TAG: &str = "bilibili_failure_class";
const CREDENTIAL_SAFE_CLIENT_DETAIL: &str =
    "Bilibili error detail omitted because credential material is configured.";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires live Bilibili network access and is intentionally outside default CI"]
async fn bilibili_live_cases_resolve_and_create_playable_hls() {
    let fixture_set = LiveFixtureSet::load();
    let run_policy = LiveRunPolicy::from_env();
    let http = reqwest::Client::new();
    let sustained_probe = run_policy.sustained_duration.map(|duration| {
        let client = reqwest::Client::builder()
            .timeout(SUSTAINED_HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("sustained probe client should build");
        SustainedProbeContext { client, duration }
    });
    let mut ran_cases = 0usize;
    let mut failed_cases = Vec::new();

    for case in fixture_set.cases.iter().filter(|case| {
        run_policy
            .filter
            .as_ref()
            .is_none_or(|filter| filter.contains(&case.id))
    }) {
        match run_policy.run_decision(case) {
            LiveRunDecision::Run => {}
            LiveRunDecision::Skip(reason) => {
                println!("skipping {}: {reason}", case.id);
                continue;
            }
        }

        ran_cases += 1;
        println!("running {}", case.id);
        let mut server = LiveTestServer::start_with_fill_mode(run_policy.full_fill).await;
        let task_tracker = LiveTaskTracker::default();
        let outcome = AssertUnwindSafe(async {
            let credential_status = fetch_bilibili_credential_status(server.channel().await).await;
            let channel = server.channel().await;
            let media_url = server.media_url.clone();
            run_live_case(
                case,
                &mut server,
                channel,
                &http,
                &media_url,
                Some(&credential_status),
                &task_tracker,
                sustained_probe.as_ref(),
                run_policy.full_fill.then_some(FullFillContext {
                    offline_duration: run_policy.offline_duration,
                    ready_file: run_policy.ready_file.clone(),
                }),
            )
            .await;
        })
        .catch_unwind()
        .await;
        if outcome.is_err() {
            failed_cases.push(case.id.clone());
        }
        server
            .shutdown(&task_tracker)
            .await
            .unwrap_or_else(|message| panic!("{}: live case teardown failed: {message}", case.id));
    }

    assert!(
        ran_cases > 0,
        "no live Bilibili e2e cases matched the filter"
    );
    assert!(
        failed_cases.is_empty(),
        "{} live Bilibili e2e case(s) failed: {}",
        failed_cases.len(),
        failed_cases.join(", ")
    );
}

// Keep independent opt-in probes and their contexts explicit at this test boundary.
#[allow(clippy::too_many_arguments)]
async fn run_live_case(
    case: &LiveCase,
    server: &mut LiveTestServer,
    channel: tonic::transport::Channel,
    http: &reqwest::Client,
    media_url: &str,
    credential_status: Option<&BilibiliCredentialStatus>,
    task_tracker: &LiveTaskTracker,
    sustained_probe: Option<&SustainedProbeContext>,
    full_fill: Option<FullFillContext>,
) {
    if full_fill.is_some() {
        assert!(
            supports_full_fill_case(case),
            "full-fill mode requires an allowlisted canonical single-result case"
        );
    }
    assert_authenticated_case_ready(case, credential_status);

    let mut task_client = TaskServiceClient::new(channel.clone());
    let source = case.source();

    let first_page = task_client
        .start_bilibili_resolution(Request::new(StartBilibiliResolutionRequest {
            url_or_id: source.clone(),
            options: Some(case.playback_options.to_proto()),
            page: Some(PageRequest {
                page_size: BILIBILI_RESOLUTION_PAGE_SIZE,
                page_token: String::new(),
            }),
            context: None,
        }))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}",
                live_failure_message(
                    case,
                    "start resolution",
                    &error.to_string(),
                    credential_status
                )
            )
        })
        .into_inner();

    let session = first_page
        .session
        .clone()
        .unwrap_or_else(|| panic!("{}: resolution did not return a session", case.id));
    assert!(
        !session.id.trim().is_empty(),
        "{}: resolution session id is empty",
        case.id
    );
    assert!(
        !session.title.trim().is_empty(),
        "{}: resolved title is empty",
        case.id
    );
    assert_eq!(
        case.expected_source_kind, session.source_kind,
        "{}: unexpected source kind",
        case.id
    );

    let (candidates, candidate_snapshot_id) =
        collect_resolution_candidates(&mut task_client, case, first_page, credential_status).await;
    if candidates.len() < case.minimum_candidates {
        panic!(
            "{}",
            live_failure_message(
                case,
                "resolve candidates",
                &format!(
                    "expected at least {} candidates, got {}",
                    case.minimum_candidates,
                    candidates.len()
                ),
                credential_status,
            )
        );
    }
    let page_snapshot = candidate_snapshot_id
        .as_deref()
        .unwrap_or_else(|| panic!("{}: candidate pages omitted snapshot identity", case.id));
    assert!(
        !page_snapshot.is_empty(),
        "{}: candidate snapshot identity is empty",
        case.id
    );
    assert_resolution_candidate_contract(case, &candidates);

    let selection = case.selection_request(&candidates, &session.default_candidate_token);
    if full_fill.is_some() {
        assert_eq!(
            1, selection.expected_result_items,
            "{}: full-fill selection must expect one result item",
            case.id
        );
        assert_eq!(
            1, selection.expected_playable_results,
            "{}: full-fill selection must expect one playable result",
            case.id
        );
    }
    let created = task_client
        .create_bilibili_task_v2(Request::new(CreateBilibiliTaskV2Request {
            session_id: session.id,
            selection: Some(selection.selection.clone()),
            execution: Some(BilibiliExecutionV2::Playback(
                case.playback_options.to_playback_spec(),
            )),
        }))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}",
                live_failure_message(
                    case,
                    "create v2 playback task",
                    &error.to_string(),
                    credential_status,
                )
            )
        })
        .into_inner();
    task_tracker.record(&created.id);

    let playable = wait_for_playable_task(
        &mut task_client,
        case,
        &created.id,
        &selection,
        credential_status,
    )
    .await;
    let source = playable
        .playback_source
        .as_ref()
        .unwrap_or_else(|| panic!("{}: playable task has no playback source", case.id));
    assert_task_playback_source_item_id(case, &playable, source);
    assert_hls_master(case, http, source, "task playback source", media_url).await;
    if let Some(probe) = sustained_probe.filter(|_| full_fill.is_none()) {
        sustain_hls_probe(
            case,
            &probe.client,
            source,
            media_url,
            probe.duration,
            Some(SustainedProbeDiagnosticContext {
                channel: &channel,
                task_id: &created.id,
                task: &playable,
            }),
        )
        .await;
    }

    let result_sources = playable_result_sources(&playable);
    assert_eq!(
        selection.expected_result_items,
        playable.result_items.len(),
        "{}: unexpected result item count",
        case.id
    );
    assert_eq!(
        selection.expected_playable_results,
        result_sources.len(),
        "{}: unexpected playable result count",
        case.id
    );
    for (index, (result_item, result_source)) in result_sources.into_iter().enumerate() {
        assert_result_playback_source_item_id(case, result_item, result_source);
        assert_hls_master(
            case,
            http,
            result_source,
            &format!("result item {} playback source", index + 1),
            media_url,
        )
        .await;
    }

    let listed_results = list_all_task_results(
        &mut task_client,
        case,
        &created.id,
        http,
        media_url,
        credential_status,
    )
    .await;
    assert_eq!(
        selection.expected_result_items,
        listed_results.len(),
        "{}: unexpected ListTaskResults item count",
        case.id
    );
    if let Some(full_fill) = full_fill {
        run_full_fill_restart_and_offline_loop(
            server,
            case,
            &created.id,
            &source.variant_id,
            media_url,
            full_fill,
            sustained_probe.map_or(Duration::from_secs(10), |probe| probe.duration),
        )
        .await;
    }
}

async fn run_full_fill_restart_and_offline_loop(
    server: &mut LiveTestServer,
    case: &LiveCase,
    task_id: &str,
    initial_variant_id: &str,
    media_url: &str,
    context: FullFillContext,
    foreground_duration: Duration,
) {
    let fill_deadline = tokio::time::Instant::now() + FULL_FILL_DEADLINE;
    let channel = server.channel().await;
    let observed_before_quiesce =
        wait_for_fill_checkpoint(&channel, case, task_id, fill_deadline).await;
    if !observed_before_quiesce
        .as_ref()
        .is_some_and(is_partial_fill_checkpoint)
    {
        println!(
            "{}: partial-resume evidence inconclusive; waiting for fill completion before restart",
            case.id
        );
        let _completed_without_partial_checkpoint =
            wait_for_completed_fill(&server.channel().await, case, task_id, fill_deadline).await;
    }
    let quiesced = server
        .restart_preserving_state_with_checkpoint(task_id)
        .await
        .unwrap_or_else(|error| panic!("{}: first same-root restart failed: {error}", case.id));
    let checkpoint_evidence =
        classify_quiesced_checkpoint(quiesced.fill_status.as_ref(), &quiesced.range_checkpoints)
            .unwrap_or_else(|message| {
                panic!("{}: quiesced checkpoint invalid: {message}", case.id)
            });
    let has_quiesced_partial = matches!(
        checkpoint_evidence,
        QuiescedCheckpointEvidence::Partial { .. }
    );
    match checkpoint_evidence {
        QuiescedCheckpointEvidence::Partial {
            status_bytes,
            durable_bytes,
            extent_count,
        } => println!(
            "{}: quiesced partial checkpoint; completed_bytes={}, durable_bytes={}, extents={}",
            case.id, status_bytes, durable_bytes, extent_count
        ),
        QuiescedCheckpointEvidence::Completed => println!(
            "{}: fill completed by the quiesced boundary; partial-resume evidence inconclusive",
            case.id
        ),
        QuiescedCheckpointEvidence::Inconclusive => println!(
            "{}: no positive durable partial checkpoint at the quiesced boundary",
            case.id
        ),
    }
    let restarted_status =
        wait_for_fill_checkpoint(&server.channel().await, case, task_id, fill_deadline).await;
    if let (Some(checkpoint), Some(restarted)) = (
        has_quiesced_partial
            .then_some(quiesced.fill_status.as_ref())
            .flatten(),
        restarted_status
            .as_ref()
            .filter(|status| is_partial_fill_checkpoint(status)),
    ) {
        assert_eq!(
            checkpoint.representation_id, restarted.representation_id,
            "{}: fill representation changed across restart checkpoint",
            case.id
        );
        assert!(
            restarted.completed_bytes >= checkpoint.completed_bytes,
            "{}: persisted fill byte count regressed across restart",
            case.id
        );
    }

    let restarted_task = get_live_task(&server.channel().await, case, task_id).await;
    let restarted_source = restarted_task.playback_source.as_ref().unwrap_or_else(|| {
        panic!(
            "{}: restarted task did not expose its LAN playback source",
            case.id
        )
    });
    let client = reqwest::Client::builder()
        .timeout(SUSTAINED_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("full-fill foreground range client should build");
    let diagnostic_channel = server.channel().await;
    sustain_hls_probe(
        case,
        &client,
        restarted_source,
        media_url,
        foreground_duration,
        Some(SustainedProbeDiagnosticContext {
            channel: &diagnostic_channel,
            task_id,
            task: &restarted_task,
        }),
    )
    .await;
    let after_restart =
        wait_for_completed_fill(&server.channel().await, case, task_id, fill_deadline).await;
    if let Some(checkpoint) = has_quiesced_partial
        .then_some(quiesced.fill_status.as_ref())
        .flatten()
    {
        assert_eq!(
            checkpoint.representation_id, after_restart.representation_id,
            "{}: completed fill changed representation after restart",
            case.id
        );
        assert!(
            after_restart.completed_bytes >= checkpoint.completed_bytes,
            "{}: completed fill byte count regressed after restart",
            case.id
        );
    }

    server
        .restart_preserving_state()
        .await
        .unwrap_or_else(|error| panic!("{}: completed-cache restart failed: {error}", case.id));
    let completed = get_live_task(&server.channel().await, case, task_id).await;
    let item_id = completed.library_item_id.trim();
    assert!(
        !item_id.is_empty(),
        "{}: completed fill did not publish a library item",
        case.id
    );
    let mut library = LibraryServiceClient::new(server.channel().await);
    let cache_only_source = library
        .get_playback_source(Request::new(GetPlaybackSourceRequest {
            item_id: item_id.to_owned(),
            variant_id: initial_variant_id.to_owned(),
        }))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{}: completed library playback source was unavailable after restart",
                case.id
            )
        })
        .into_inner();
    assert_eq!(
        PlaybackProtocol::Hls,
        cache_only_source.protocol(),
        "{}: completed library source is not HLS",
        case.id
    );
    let source_url = assert_lan_media_url(
        case,
        &cache_only_source.uri,
        media_url,
        "completed library playback source",
    );
    assert_clean_lan_url(case, &source_url);
    let session_id = hls_session_id_from_master_uri(case, &source_url);
    assert_completed_session_has_no_upstream_urls(server.temp_root_path(), &session_id)
        .unwrap_or_else(|_| {
            panic!(
                "{}: completed cache session retained upstream URL or header material",
                case.id
            )
        });

    let http = reqwest::Client::builder()
        .timeout(SUSTAINED_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("offline playback client should build");
    let payload = read_completed_hls_payloads(case, &http, &cache_only_source, media_url).await;
    assert!(
        payload.total_bytes > 0,
        "{}: offline HLS walk read no media bytes",
        case.id
    );
    let cache_checksums_before =
        cached_media_checksums(server.temp_root_path(), &session_id, &payload.resources)
            .unwrap_or_else(|_| {
                panic!(
                    "{}: completed cache files could not be checksummed",
                    case.id
                )
            });
    if let Some(path) = context.ready_file {
        let offline_duration = context
            .offline_duration
            .expect("ready file configuration requires sustained offline reads");
        let ready = LanPlaybackReady {
            phase: "cache-only-ready",
            uri: cache_only_source.uri.clone(),
            case_id: case.id.clone(),
            task_id: task_id.to_owned(),
            total_bytes: payload.total_bytes,
            expires_in_seconds: offline_duration.as_secs(),
        };
        let bytes = serde_json::to_vec(&ready).expect("ready document should serialize");
        server
            .publish_ready_file(path, bytes)
            .unwrap_or_else(|error| {
                panic!("{}: ready document publication failed: {error}", case.id)
            });
    }
    let aggregate_bytes = if let Some(duration) = context.offline_duration {
        sustain_offline_cache_reads(
            case,
            &http,
            &payload.resources,
            media_url,
            duration,
            payload.total_bytes,
        )
        .await
    } else {
        payload.total_bytes
    };
    let cache_checksums_after =
        cached_media_checksums(server.temp_root_path(), &session_id, &payload.resources)
            .unwrap_or_else(|_| {
                panic!(
                    "{}: completed cache files could not be checksummed after offline reads",
                    case.id
                )
            });
    assert_eq!(
        cache_checksums_before, cache_checksums_after,
        "{}: completed cache-file checksum changed during offline reads",
        case.id
    );
    println!(
        "{}: cache-only HTTP validation completed; resources={}, bytes={}, sha256={}",
        case.id,
        payload.resources.len(),
        aggregate_bytes,
        payload.sha256
    );
}

fn is_partial_fill_checkpoint(
    status: &tvos_net_player_cache_server::generated::tvos_net_player::v1::HlsCacheFillStatus,
) -> bool {
    let resumable_state = matches!(
        HlsCacheFillState::try_from(status.state),
        Ok(HlsCacheFillState::Queued
            | HlsCacheFillState::Filling
            | HlsCacheFillState::Preempted
            | HlsCacheFillState::Retrying)
    );
    resumable_state
        && status.completed_bytes > 0
        && (!status.total_bytes_known || status.completed_bytes < status.total_bytes)
}

fn snapshot_range_checkpoints(root: &Path) -> Result<RangeCheckpointSnapshot, ()> {
    let cache_root = root.join(".tvos-net-player/hls");
    let entries = match fs::read_dir(&cache_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RangeCheckpointSnapshot::default());
        }
        Err(_) => return Err(()),
    };
    if fs::symlink_metadata(&cache_root)
        .map_err(|_| ())?
        .file_type()
        .is_symlink()
    {
        return Err(());
    }

    let mut snapshot = RangeCheckpointSnapshot::default();
    let mut session_count = 0usize;
    let mut json_count = 0usize;
    let mut total_json_bytes = 0u64;
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        let file_type = entry.file_type().map_err(|_| ())?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        session_count += 1;
        if session_count > 32 {
            return Err(());
        }
        let session_id = entry.file_name().into_string().map_err(|_| ())?;
        let files = fs::read_dir(entry.path()).map_err(|_| ())?;
        for file in files {
            let file = file.map_err(|_| ())?;
            if file.file_type().map_err(|_| ())?.is_symlink()
                || file
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "json")
            {
                continue;
            }
            json_count += 1;
            if json_count > 512 {
                return Err(());
            }
            let metadata = file.metadata().map_err(|_| ())?;
            if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
                return Err(());
            }
            total_json_bytes = total_json_bytes.checked_add(metadata.len()).ok_or(())?;
            if total_json_bytes > 8 * 1024 * 1024 {
                return Err(());
            }
            let bytes = fs::read(file.path()).map_err(|_| ())?;
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(extents) = value.get("extents").and_then(serde_json::Value::as_array) else {
                continue;
            };
            let resource_id = value
                .get("resource_id")
                .and_then(serde_json::Value::as_str)
                .ok_or(())?;
            let representation = value
                .get("representation_digest")
                .and_then(serde_json::Value::as_str)
                .ok_or(())?;
            let key = format!("{session_id}/{resource_id}/{representation}");
            let mut verified = Vec::with_capacity(extents.len());
            for extent in extents {
                let start = extent
                    .get("start")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or(())?;
                let end = extent
                    .get("end")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or(())?;
                let sha256 = extent
                    .get("sha256")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(())?;
                if end < start
                    || sha256.len() != 64
                    || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(());
                }
                let extent_bytes = end
                    .checked_sub(start)
                    .and_then(|length| length.checked_add(1))
                    .ok_or(())?;
                snapshot.durable_bytes =
                    snapshot.durable_bytes.checked_add(extent_bytes).ok_or(())?;
                verified.push(RangeExtentCheckpoint {
                    start,
                    end,
                    sha256: sha256.to_owned(),
                });
            }
            if !verified.is_empty() {
                snapshot.manifests.insert(key, verified);
            }
        }
    }
    Ok(snapshot)
}

fn classify_quiesced_checkpoint(
    status: Option<
        &tvos_net_player_cache_server::generated::tvos_net_player::v1::HlsCacheFillStatus,
    >,
    snapshot: &RangeCheckpointSnapshot,
) -> Result<QuiescedCheckpointEvidence, &'static str> {
    if status.is_some_and(|status| status.state == HlsCacheFillState::Completed as i32) {
        return Ok(QuiescedCheckpointEvidence::Completed);
    }
    let Some(status) = status.filter(|status| is_partial_fill_checkpoint(status)) else {
        return Ok(QuiescedCheckpointEvidence::Inconclusive);
    };
    if status.representation_id.trim().is_empty() {
        return Err("typed partial status omitted the selected representation identity");
    }
    if snapshot.durable_bytes == 0 {
        return Err("typed partial status had no quiesced durable range extents");
    }
    if snapshot.durable_bytes < status.completed_bytes {
        return Err("quiesced range extents were below the typed progress lower bound");
    }
    Ok(QuiescedCheckpointEvidence::Partial {
        status_bytes: status.completed_bytes,
        durable_bytes: snapshot.durable_bytes,
        extent_count: snapshot.manifests.values().map(Vec::len).sum(),
    })
}

fn hls_session_id_from_master_uri(case: &LiveCase, url: &Url) -> String {
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        segments.len() == 3 && segments[0] == "hls" && segments[2] == "master.m3u8",
        "{}: completed LAN playback URI did not identify a canonical HLS session",
        case.id
    );
    segments[1].to_owned()
}

fn assert_completed_session_has_no_upstream_urls(root: &Path, session_id: &str) -> Result<(), ()> {
    let session_path = root
        .join(".tvos-net-player/hls")
        .join(session_id)
        .join("session.json");
    let metadata = fs::symlink_metadata(&session_path).map_err(|_| ())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 2 * 1024 * 1024
    {
        return Err(());
    }
    let bytes = fs::read(session_path).map_err(|_| ())?;
    let value = serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|_| ())?;
    if persisted_session_contains_upstream_material(&value) {
        return Err(());
    }
    Ok(())
}

fn persisted_session_contains_upstream_material(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, value)| {
            let normalized = key.to_ascii_lowercase();
            if normalized == "headers" {
                return match value {
                    serde_json::Value::Array(values) => !values.is_empty(),
                    serde_json::Value::Object(values) => !values.is_empty(),
                    serde_json::Value::Null => false,
                    _ => true,
                };
            }
            if normalized == "url" || normalized.ends_with("_url") || normalized == "backup_urls" {
                return match value {
                    serde_json::Value::String(value) => !value.trim().is_empty(),
                    serde_json::Value::Array(values) => !values.is_empty(),
                    serde_json::Value::Null => false,
                    _ => true,
                };
            }
            persisted_session_contains_upstream_material(value)
        }),
        serde_json::Value::Array(values) => values
            .iter()
            .any(persisted_session_contains_upstream_material),
        _ => false,
    }
}

async fn get_live_task(
    channel: &tonic::transport::Channel,
    case: &LiveCase,
    task_id: &str,
) -> Task {
    TaskServiceClient::new(channel.clone())
        .get_task(Request::new(GetTaskRequest {
            id: task_id.to_owned(),
        }))
        .await
        .unwrap_or_else(|_| panic!("{}: safe task status query failed", case.id))
        .into_inner()
}

async fn wait_for_fill_checkpoint(
    channel: &tonic::transport::Channel,
    case: &LiveCase,
    task_id: &str,
    fill_deadline: tokio::time::Instant,
) -> Option<tvos_net_player_cache_server::generated::tvos_net_player::v1::HlsCacheFillStatus> {
    let deadline = fill_deadline.min(tokio::time::Instant::now() + Duration::from_secs(30));
    let mut latest = None;
    loop {
        let task = get_live_task(channel, case, task_id).await;
        let status = task.hls_cache_fill_status;
        if let Some(status) = status {
            if status.state == HlsCacheFillState::BlockedQuota as i32 {
                panic!(
                    "{}: fill exceeded its 1 GiB cache budget; refusing to increase the limit",
                    case.id
                );
            }
            if status.state == HlsCacheFillState::Completed as i32 || status.completed_bytes > 0 {
                return Some(status);
            }
            latest = Some(status);
        }
        if tokio::time::Instant::now() >= deadline {
            return latest;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_completed_fill(
    channel: &tonic::transport::Channel,
    case: &LiveCase,
    task_id: &str,
    deadline: tokio::time::Instant,
) -> tvos_net_player_cache_server::generated::tvos_net_player::v1::HlsCacheFillStatus {
    loop {
        let task = get_live_task(channel, case, task_id).await;
        if let Some(status) = task.hls_cache_fill_status {
            if status.state == HlsCacheFillState::Completed as i32 {
                assert!(
                    status.completed_bytes > 0,
                    "{}: completed fill reported zero bytes",
                    case.id
                );
                assert!(
                    status.total_bytes_known,
                    "{}: completed fill omitted total bytes",
                    case.id
                );
                assert_eq!(
                    status.total_bytes, status.completed_bytes,
                    "{}: completed fill byte totals differ",
                    case.id
                );
                assert!(
                    !status.representation_id.trim().is_empty(),
                    "{}: completed fill omitted representation identity",
                    case.id
                );
                return status;
            }
            if matches!(
                HlsCacheFillState::try_from(status.state),
                Ok(HlsCacheFillState::BlockedQuota
                    | HlsCacheFillState::Failed
                    | HlsCacheFillState::SourceUnavailable
                    | HlsCacheFillState::Cancelled)
            ) {
                panic!(
                    "{}: fill stopped or exceeded its 1 GiB cache budget in state {:?}",
                    case.id,
                    HlsCacheFillState::try_from(status.state).ok()
                );
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{}: full fill did not complete before the 1200-second deadline",
            case.id
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn collect_resolution_candidates(
    task_client: &mut TaskServiceClient<tonic::transport::Channel>,
    case: &LiveCase,
    first_page: BilibiliResolutionPage,
    credential_status: Option<&BilibiliCredentialStatus>,
) -> (Vec<BilibiliResolutionCandidate>, Option<String>) {
    let session = first_page
        .session
        .clone()
        .unwrap_or_else(|| panic!("{}: resolution page omitted session", case.id));
    let first_page_info = first_page
        .page_info
        .clone()
        .unwrap_or_else(|| panic!("{}: resolution page omitted pagination metadata", case.id));
    let snapshot_id = first_page_info.snapshot_id.clone();
    let total_size = first_page_info.total_size;
    let mut candidates = first_page.candidates;
    let mut page_token = first_page_info.next_page_token;

    while !page_token.is_empty() {
        let page = task_client
            .list_bilibili_resolution_candidates(Request::new(
                ListBilibiliResolutionCandidatesRequest {
                    session_id: session.id.clone(),
                    page: Some(PageRequest {
                        page_size: BILIBILI_RESOLUTION_PAGE_SIZE,
                        page_token,
                    }),
                },
            ))
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}",
                    live_failure_message(
                        case,
                        "list resolution candidates",
                        &error.to_string(),
                        credential_status
                    )
                )
            })
            .into_inner();
        assert_resolution_session_consistent(case, &session, page.session.as_ref());
        let page_info = page
            .page_info
            .unwrap_or_else(|| panic!("{}: candidate page omitted pagination metadata", case.id));
        assert_eq!(
            snapshot_id, page_info.snapshot_id,
            "{}: candidate snapshot changed between pages",
            case.id
        );
        assert_eq!(
            total_size, page_info.total_size,
            "{}: candidate total changed between pages",
            case.id
        );
        candidates.extend(page.candidates);
        page_token = page_info.next_page_token;
    }
    assert_eq!(
        total_size as usize,
        candidates.len(),
        "{}: candidate pagination did not cover the snapshot",
        case.id
    );
    (candidates, Some(snapshot_id))
}

fn assert_resolution_session_consistent(
    case: &LiveCase,
    expected: &tvos_net_player_cache_server::generated::tvos_net_player::v1::BilibiliResolutionSession,
    actual: Option<
        &tvos_net_player_cache_server::generated::tvos_net_player::v1::BilibiliResolutionSession,
    >,
) {
    let actual = actual.unwrap_or_else(|| panic!("{}: candidate page omitted session", case.id));
    assert_eq!(
        expected.id, actual.id,
        "{}: resolution session changed between pages",
        case.id
    );
    assert_eq!(
        expected.source_kind, actual.source_kind,
        "{}: resolution source kind changed between pages",
        case.id
    );
    assert_eq!(
        expected.title, actual.title,
        "{}: resolution title changed between pages",
        case.id
    );
}

async fn list_all_task_results(
    task_client: &mut TaskServiceClient<tonic::transport::Channel>,
    case: &LiveCase,
    task_id: &str,
    http: &reqwest::Client,
    media_url: &str,
    credential_status: Option<&BilibiliCredentialStatus>,
) -> Vec<TaskResult> {
    let mut page_token = String::new();
    let mut snapshot_id = None;
    let mut total_size = None;
    let mut output_revision = None;
    let mut results = Vec::new();
    loop {
        let page = task_client
            .list_task_results(Request::new(ListTaskResultsRequest {
                task_id: task_id.to_owned(),
                page: Some(PageRequest {
                    page_size: BILIBILI_TASK_RESULT_PAGE_SIZE,
                    page_token,
                }),
            }))
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}",
                    live_failure_message(
                        case,
                        "list task results",
                        &error.to_string(),
                        credential_status
                    )
                )
            })
            .into_inner();
        let page_info = page
            .page_info
            .unwrap_or_else(|| panic!("{}: task result page omitted pagination metadata", case.id));
        if let Some(expected) = snapshot_id.as_deref() {
            assert_eq!(
                expected, page_info.snapshot_id,
                "{}: task result snapshot changed between pages",
                case.id
            );
            assert_eq!(
                total_size,
                Some(page_info.total_size),
                "{}: task result total changed between pages",
                case.id
            );
            assert_eq!(
                output_revision,
                Some(page.output_revision),
                "{}: task output revision changed between pages",
                case.id
            );
        } else {
            assert!(
                !page_info.snapshot_id.is_empty(),
                "{}: task result snapshot identity is empty",
                case.id
            );
            snapshot_id = Some(page_info.snapshot_id.clone());
            total_size = Some(page_info.total_size);
            output_revision = Some(page.output_revision);
        }
        for result in &page.results {
            if let Some(source) = result.playback_source.as_ref() {
                assert_hls_master(
                    case,
                    http,
                    source,
                    "listed task result playback source",
                    media_url,
                )
                .await;
            }
            for artifact in &result.artifacts {
                assert_artifact_reference_safe(case, artifact, media_url);
            }
        }
        results.extend(page.results);
        page_token = page_info.next_page_token;
        if page_token.is_empty() {
            break;
        }
    }
    assert_eq!(
        total_size.unwrap_or_default() as usize,
        results.len(),
        "{}: task result pagination did not cover the snapshot",
        case.id
    );
    results
}

fn assert_artifact_reference_safe(case: &LiveCase, artifact: &TaskArtifact, media_url: &str) {
    let Some(resource) = artifact.resource.as_ref() else {
        return;
    };
    assert!(
        !resource.id.trim().is_empty(),
        "{}: task artifact resource id is empty",
        case.id
    );
    let url = Url::parse(&resource.uri).unwrap_or_else(|_| {
        panic!(
            "{}: task artifact resource URI is not an absolute server URL",
            case.id
        )
    });
    let expected = Url::parse(media_url).expect("configured media listener URL should parse");
    assert_eq!(
        (
            expected.scheme(),
            expected.host_str(),
            expected.port_or_known_default()
        ),
        (url.scheme(), url.host_str(), url.port_or_known_default()),
        "{}: task artifact reference is not owned by the LAN media listener",
        case.id
    );
    assert!(
        url.username().is_empty() && url.password().is_none(),
        "{}: task artifact reference contains URL credentials",
        case.id
    );
    assert!(
        url.query().is_none() && url.fragment().is_none(),
        "{}: task artifact reference contains query or fragment data",
        case.id
    );
    assert!(
        !url.path().contains(".."),
        "{}: task artifact reference contains traversal syntax",
        case.id
    );
    let path = url.path().to_ascii_lowercase();
    assert!(
        !path.contains("/users/") && !path.contains("/private/") && !path.contains("\\"),
        "{}: task artifact reference exposes a filesystem path",
        case.id
    );
}

async fn wait_for_playable_task(
    task_client: &mut TaskServiceClient<tonic::transport::Channel>,
    case: &LiveCase,
    task_id: &str,
    selection: &LiveSelectionRequest,
    credential_status: Option<&BilibiliCredentialStatus>,
) -> Task {
    let timeout = Duration::from_secs(case.timeout_seconds.unwrap_or(90));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let task = task_client
            .get_task(Request::new(GetTaskRequest {
                id: task_id.to_owned(),
            }))
            .await
            .unwrap_or_else(|error| panic!("{}: get task failed: {error}", case.id))
            .into_inner();

        match task.state() {
            TaskState::Playable | TaskState::Completed
                if task_has_expected_playable_sources(&task, selection) =>
            {
                return task;
            }
            TaskState::Failed | TaskState::Cancelled => {
                panic!(
                    "{}",
                    live_task_failure_message(
                        case,
                        &format!("task ended in {:?}", task.state()),
                        &task,
                        credential_status,
                    )
                );
            }
            _ if tokio::time::Instant::now() >= deadline => {
                panic!(
                    "{}",
                    live_task_failure_message(
                        case,
                        &format!(
                            "task did not become playable within {:?}; last state {:?}",
                            timeout,
                            task.state()
                        ),
                        &task,
                        credential_status,
                    )
                );
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

fn task_has_expected_playable_sources(task: &Task, selection: &LiveSelectionRequest) -> bool {
    if task.result_items.len() != selection.expected_result_items {
        return false;
    }

    playable_result_sources(task).len() == selection.expected_playable_results
        && task.playback_source.is_some()
}

fn playable_result_sources(task: &Task) -> Vec<(&BilibiliTaskResultItem, &PlaybackSource)> {
    task.result_items
        .iter()
        .filter(|item| {
            item.state == i32::from(TaskState::Playable)
                || item.state == i32::from(TaskState::Completed)
        })
        .filter_map(|item| item.playback_source.as_ref().map(|source| (item, source)))
        .collect()
}

fn assert_task_playback_source_item_id(case: &LiveCase, task: &Task, source: &PlaybackSource) {
    let expected_item_id = if task.state == i32::from(TaskState::Completed) {
        task.library_item_id.trim()
    } else {
        task.id.trim()
    };
    assert!(
        !expected_item_id.is_empty(),
        "{}: task has no expected playback source item id",
        case.id
    );
    assert_eq!(
        expected_item_id, source.item_id,
        "{}: task playback source item id mismatch",
        case.id
    );
}

fn assert_result_playback_source_item_id(
    case: &LiveCase,
    result_item: &BilibiliTaskResultItem,
    source: &PlaybackSource,
) {
    let expected_item_id = if result_item.state == i32::from(TaskState::Completed) {
        result_item.library_item_id.trim()
    } else {
        result_item.id.trim()
    };
    assert!(
        !expected_item_id.is_empty(),
        "{}: result item {} has no expected playback source item id",
        case.id,
        result_item.id
    );
    assert_eq!(
        expected_item_id, source.item_id,
        "{}: result item {} playback source item id mismatch",
        case.id, result_item.id
    );
}

async fn assert_hls_master(
    case: &LiveCase,
    http: &reqwest::Client,
    source: &PlaybackSource,
    label: &str,
    media_url: &str,
) {
    assert_eq!(
        PlaybackProtocol::Hls,
        source.protocol(),
        "{}: {label} is not HLS",
        case.id
    );

    let source_url = assert_lan_media_url(case, &source.uri, media_url, label);

    let response = http.get(&source.uri).send().await.unwrap_or_else(|error| {
        panic!(
            "{}: {label} HLS master request failed: {}",
            case.id,
            error.without_url()
        )
    });
    assert_eq!(
        StatusCode::OK,
        response.status(),
        "{}: {label} HLS master returned unexpected status",
        case.id
    );
    let playlist = response.text().await.unwrap_or_else(|error| {
        panic!(
            "{}: {label} HLS master body failed: {}",
            case.id,
            error.without_url()
        )
    });
    assert!(
        playlist.contains("#EXTM3U"),
        "{}: {label} HLS master is not an m3u8 playlist",
        case.id
    );
    let media_playlist_urls =
        assert_playlist_stays_on_lan(case, &source_url, &playlist, media_url, label);
    for media_playlist_url in media_playlist_urls {
        let nested_label = format!("{label} media playlist");
        let media_playlist_origin = lan_media_origin_for_diagnostic(&media_playlist_url);
        let response = http
            .get(media_playlist_url.clone())
            .send()
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{}: {nested_label} request failed for origin={media_playlist_origin}: {}",
                    case.id,
                    error.without_url()
                )
            });
        assert_eq!(
            StatusCode::OK,
            response.status(),
            "{}: {nested_label} returned non-OK status for origin={media_playlist_origin}",
            case.id
        );
        let media_playlist = response.text().await.unwrap_or_else(|error| {
            panic!(
                "{}: {nested_label} response body failed for origin={media_playlist_origin}: {}",
                case.id,
                error.without_url()
            )
        });
        assert!(
            media_playlist.starts_with("#EXTM3U"),
            "{}: {nested_label} is not an m3u8 playlist for origin={media_playlist_origin}",
            case.id
        );
        assert_playlist_stays_on_lan(
            case,
            &media_playlist_url,
            &media_playlist,
            media_url,
            &nested_label,
        );
    }
}

async fn sustain_hls_probe(
    case: &LiveCase,
    http: &reqwest::Client,
    source: &PlaybackSource,
    media_url: &str,
    duration: Duration,
    diagnostic: Option<SustainedProbeDiagnosticContext<'_>>,
) {
    let deadline = tokio::time::Instant::now() + duration;
    let source_url =
        assert_lan_media_url(case, &source.uri, media_url, "sustained playback source");
    let master = fetch_sustained_playlist(case, http, &source_url, media_url, "master").await;
    let child_urls = assert_playlist_stays_on_lan(
        case,
        &source_url,
        &master,
        media_url,
        "sustained master playlist",
    );
    let child_roles = sustained_child_playlist_roles(&master, &source_url);
    let child_urls = unique_child_playlist_urls(child_urls, &source_url);
    let mut playlists = SustainedPlaylistSet::new(child_urls);
    assert!(
        playlists.len() <= SUSTAINED_MAX_CHILD_PLAYLISTS,
        "{}: sustained master playlist exceeded the child playlist limit",
        case.id
    );

    let mut probes = 0usize;
    for index in 0..playlists.len() {
        set_sustained_child_role(playlists.playlist_mut(index), &child_roles);
        probe_sustained_child_playlist(
            case,
            http,
            media_url,
            playlists.playlist_mut(index),
            source,
            diagnostic.as_ref(),
        )
        .await;
        playlists.record_range(index);
        probes += 1;
    }
    assert!(
        playlists.all_initial_children_ranged(),
        "{}: not every initial child playlist received a media range probe",
        case.id
    );

    while tokio::time::Instant::now() < deadline {
        let master = fetch_sustained_playlist(case, http, &source_url, media_url, "master").await;
        assert_playlist_stays_on_lan(
            case,
            &source_url,
            &master,
            media_url,
            "sustained master playlist",
        );
        let index = playlists.next_index().unwrap_or_else(|| {
            panic!(
                "{}: sustained master playlist had no child playlists",
                case.id
            )
        });
        set_sustained_child_role(playlists.playlist_mut(index), &child_roles);
        probe_sustained_child_playlist(
            case,
            http,
            media_url,
            playlists.playlist_mut(index),
            source,
            diagnostic.as_ref(),
        )
        .await;
        playlists.record_range(index);
        probes += 1;
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    assert!(probes > 0, "{}: sustained probe did not run", case.id);
    println!(
        "{}: sustained LAN HLS probe completed ({probes} rounds)",
        case.id
    );
}

async fn probe_sustained_child_playlist(
    case: &LiveCase,
    http: &reqwest::Client,
    media_url: &str,
    playlist: &mut SustainedChildPlaylist,
    source: &PlaybackSource,
    diagnostic: Option<&SustainedProbeDiagnosticContext<'_>>,
) {
    let media_playlist =
        fetch_sustained_playlist(case, http, &playlist.url, media_url, "media").await;
    assert_playlist_stays_on_lan(
        case,
        &playlist.url,
        &media_playlist,
        media_url,
        "sustained media playlist",
    );
    let resources = hls_probe_resources(&media_playlist).unwrap_or_else(|_| {
        panic!(
            "{}: sustained media playlist has invalid byte ranges",
            case.id
        )
    });
    let request =
        next_sustained_probe_request(&resources, &mut playlist.cursor).unwrap_or_else(|| {
            panic!(
                "{}: sustained media playlist has no media segments",
                case.id
            )
        });
    let url = playlist.url.join(&request.uri).unwrap_or_else(|_| {
        panic!(
            "{}: sustained media playlist contains an invalid segment URI",
            case.id
        )
    });
    assert_lan_media_url(case, url.as_str(), media_url, "sustained media segment");
    fetch_sustained_media_bytes(
        case,
        http,
        &url,
        &request.range,
        source,
        diagnostic,
        &playlist.role,
    )
    .await;
}

fn unique_child_playlist_urls(urls: Vec<Url>, fallback: &Url) -> Vec<Url> {
    let mut seen = HashSet::new();
    let urls = if urls.is_empty() {
        vec![fallback.clone()]
    } else {
        urls
    };
    urls.into_iter()
        .filter(|url| seen.insert(url.as_str().to_owned()))
        .collect()
}

fn sustained_child_playlist_roles(master: &str, master_url: &Url) -> HashMap<String, String> {
    let mut audio_urls = HashMap::<String, String>::new();
    let mut variants = Vec::<(Option<String>, String)>::new();
    let mut pending_audio_group = None;
    let mut pending_stream = false;

    for line in master.lines().map(str::trim) {
        if line.starts_with("#EXT-X-MEDIA:") && line.contains("TYPE=AUDIO") {
            if let (Some(group), Some(uri)) =
                (hls_attribute(line, "GROUP-ID"), hls_attribute(line, "URI"))
                && let Ok(url) = master_url.join(&uri)
            {
                audio_urls.insert(group, url.to_string());
            }
        } else if line.starts_with("#EXT-X-STREAM-INF:") {
            pending_audio_group = hls_attribute(line, "AUDIO");
            pending_stream = true;
        } else if pending_stream && !line.is_empty() && !line.starts_with('#') {
            if let Ok(url) = master_url.join(line) {
                variants.push((pending_audio_group.take(), url.to_string()));
            }
            pending_stream = false;
        }
    }

    let mut roles = HashMap::new();
    for (index, (audio_group, video_url)) in variants.iter().enumerate() {
        let role = if index == 0 {
            "primary".to_owned()
        } else {
            format!("alternate_{index}")
        };
        roles.insert(video_url.clone(), format!("video_{role}"));
        if let Some(audio_url) = audio_group.as_ref().and_then(|group| audio_urls.get(group)) {
            roles.insert(audio_url.clone(), format!("audio_{role}"));
        }
    }
    roles
}

fn hls_attribute(line: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=\"");
    let start = line.find(&marker)? + marker.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_owned())
}

fn set_sustained_child_role(
    playlist: &mut SustainedChildPlaylist,
    roles: &HashMap<String, String>,
) {
    playlist.role = roles
        .get(playlist.url.as_str())
        .cloned()
        .unwrap_or_else(|| "unresolved_child".to_owned());
}

async fn fetch_sustained_playlist(
    case: &LiveCase,
    http: &reqwest::Client,
    url: &Url,
    media_url: &str,
    label: &str,
) -> String {
    assert_lan_media_url(case, url.as_str(), media_url, "sustained playlist");
    let origin = lan_media_origin_for_diagnostic(url);
    let response = http.get(url.clone()).send().await.unwrap_or_else(|error| {
        panic!(
            "{}: sustained {label} playlist request failed for origin={origin}: {}",
            case.id,
            error.without_url()
        )
    });
    assert_eq!(
        StatusCode::OK,
        response.status(),
        "{}: sustained {label} playlist returned non-OK status for origin={origin}",
        case.id
    );
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.unwrap_or_else(|error| {
        panic!(
            "{}: sustained {label} playlist body failed for origin={origin}: {}",
            case.id,
            error.without_url()
        )
    }) {
        assert!(
            body.len().saturating_add(chunk.len()) <= SUSTAINED_PLAYLIST_LIMIT,
            "{}: sustained {label} playlist exceeded the size limit for origin={origin}",
            case.id
        );
        body.extend_from_slice(&chunk);
    }
    let playlist = String::from_utf8(body).unwrap_or_else(|_| {
        panic!(
            "{}: sustained {label} playlist is not UTF-8 for origin={origin}",
            case.id
        )
    });
    assert!(
        playlist.starts_with("#EXTM3U"),
        "{}: sustained {label} response is not an HLS playlist for origin={origin}",
        case.id
    );
    playlist
}

async fn fetch_sustained_media_bytes(
    case: &LiveCase,
    http: &reqwest::Client,
    url: &Url,
    range: &ByteRangeRequest,
    source: &PlaybackSource,
    diagnostic: Option<&SustainedProbeDiagnosticContext<'_>>,
    child_role: &str,
) {
    let origin = lan_media_origin_for_diagnostic(url);
    let response = http
        .get(url.clone())
        .header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", range.start, range.end),
        )
        .send()
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}: sustained media range request failed for origin={origin}: {}",
                case.id,
                error.without_url()
            )
        });
    if response.status() != StatusCode::PARTIAL_CONTENT {
        let status = response.status();
        let body = bounded_failure_body(response).await;
        let local = match diagnostic {
            Some(diagnostic) => {
                sustained_failure_local_state(diagnostic, source, url, child_role).await
            }
            None => "local_runtime=unavailable".to_owned(),
        };
        panic!(
            "{}: sustained media range returned status={} expected=206 origin={} requested_bytes={}-{} response_body_bytes={} response_body_truncated={} response_body_read={} {};",
            case.id,
            status.as_u16(),
            origin,
            range.start,
            range.end,
            body.bytes,
            body.truncated,
            body.read_status,
            local,
        );
    }
    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(content_range_bounds)
        .unwrap_or_else(|| {
            panic!(
                "{}: sustained media range omitted a valid Content-Range for origin={origin}",
                case.id
            )
        });
    assert_eq!(
        (range.start, range.end),
        content_range,
        "{}: sustained media range bounds differed from the requested interval for origin={origin}",
        case.id
    );
    let mut response = response;
    let expected_bytes = usize::try_from(range.end - range.start + 1)
        .expect("sustained range is bounded by the 64 KiB read limit");
    let mut received_bytes = 0usize;
    while received_bytes < expected_bytes {
        let Some(chunk) = response.chunk().await.unwrap_or_else(|error| {
            panic!(
                "{}: sustained media bytes failed for origin={origin}: {}",
                case.id,
                error.without_url()
            )
        }) else {
            break;
        };
        received_bytes = received_bytes
            .checked_add(chunk.len())
            .expect("received media byte count should fit in memory bounds");
        assert!(
            received_bytes <= expected_bytes,
            "{}: sustained media range returned more than the requested byte count for origin={origin}",
            case.id
        );
    }
    assert_eq!(
        expected_bytes, received_bytes,
        "{}: sustained media range returned fewer than the requested bytes for origin={origin}",
        case.id
    );
}

struct SustainedProbeDiagnosticContext<'a> {
    channel: &'a tonic::transport::Channel,
    task_id: &'a str,
    task: &'a Task,
}

struct BoundedFailureBody {
    bytes: usize,
    truncated: bool,
    read_status: &'static str,
}

async fn bounded_failure_body(response: reqwest::Response) -> BoundedFailureBody {
    let result = tokio::time::timeout(SUSTAINED_DIAGNOSTIC_TIMEOUT, async move {
        let mut response = response;
        let mut bytes = 0usize;
        let mut truncated = false;
        while let Some(chunk) = response.chunk().await? {
            let remaining = SUSTAINED_FAILURE_BODY_LIMIT.saturating_sub(bytes);
            bytes += chunk.len().min(remaining);
            if chunk.len() > remaining || bytes == SUSTAINED_FAILURE_BODY_LIMIT {
                truncated = true;
                break;
            }
        }
        Ok::<_, reqwest::Error>((bytes, truncated))
    })
    .await;
    match result {
        Ok(Ok((bytes, truncated))) => BoundedFailureBody {
            bytes,
            truncated,
            read_status: "complete",
        },
        Ok(Err(_)) => BoundedFailureBody {
            bytes: 0,
            truncated: false,
            read_status: "body_error",
        },
        Err(_) => BoundedFailureBody {
            bytes: 0,
            truncated: true,
            read_status: "timeout",
        },
    }
}

async fn sustained_failure_local_state(
    diagnostic: &SustainedProbeDiagnosticContext<'_>,
    source: &PlaybackSource,
    requested_url: &Url,
    child_role: &str,
) -> String {
    let latest_task = tokio::time::timeout(SUSTAINED_DIAGNOSTIC_TIMEOUT, async {
        TaskServiceClient::new(diagnostic.channel.clone())
            .get_task(Request::new(GetTaskRequest {
                id: diagnostic.task_id.to_owned(),
            }))
            .await
    })
    .await;
    let task_snapshot = match latest_task {
        Ok(Ok(response)) => format_task_diagnostic(&response.into_inner()),
        Ok(Err(_)) => format!(
            "task_query=grpc_error fallback=[{}]",
            format_task_diagnostic(diagnostic.task)
        ),
        Err(_) => format!(
            "task_query=timeout fallback=[{}]",
            format_task_diagnostic(diagnostic.task)
        ),
    };

    let Some((_session_id, resource_id)) = hls_resource_path_parts(requested_url) else {
        return format!(
            "task_snapshot=[{}] requested_resource=unparsed child_role={} source_variant_id={} runtime_generation=not_exposed runtime_policy=not_exposed cache_key=not_exposed",
            task_snapshot,
            safe_diagnostic_value(child_role),
            safe_diagnostic_value(&source.variant_id),
        );
    };
    let kind = if child_role.starts_with("audio_") {
        "audio"
    } else if child_role.starts_with("video_") {
        "video"
    } else {
        "unknown"
    };
    let variant_role = if child_role.ends_with("_primary") {
        "primary"
    } else if child_role.contains("_alternate_") {
        "alternate"
    } else {
        "unresolved"
    };
    format!(
        "task_snapshot=[{}] resource_kind={} resource_id={} child_role={} variant_role={} task_source=primary_playback_source source_variant_id={} cache_key=not_exposed runtime_generation=not_exposed runtime_policy=not_exposed",
        task_snapshot,
        kind,
        safe_diagnostic_value(resource_id),
        safe_diagnostic_value(child_role),
        variant_role,
        safe_diagnostic_value(&source.variant_id),
    )
}

fn format_task_diagnostic(task: &Task) -> String {
    let state = TaskState::try_from(task.state)
        .map(|state| format!("{state:?}"))
        .unwrap_or_else(|_| format!("unknown:{}", task.state));
    let revision = task
        .output_summary
        .as_ref()
        .map_or(0, |summary| summary.revision);
    format!(
        "id={} state={} output_revision={} result_items={} fill_state={}",
        safe_diagnostic_value(&task.id),
        state,
        revision,
        task.result_items.len(),
        task.hls_cache_fill_status
            .as_ref()
            .map(|status| format!("{}:{}", status.state, status.completed_bytes))
            .unwrap_or_else(|| "none".to_owned()),
    )
}

fn hls_resource_path_parts(url: &Url) -> Option<(&str, &str)> {
    let segments = url.path_segments()?.collect::<Vec<_>>();
    match segments.as_slice() {
        ["hls", session_id, "segments", resource_id] => Some((session_id, resource_id)),
        _ => None,
    }
}

fn safe_diagnostic_value(value: &str) -> String {
    value
        .chars()
        .take(128)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ByteRangeRequest {
    start: u64,
    end: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HlsProbeResource {
    uri: String,
    range: Option<ByteRangeRequest>,
    initialization: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SustainedProbeRequest {
    uri: String,
    range: ByteRangeRequest,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SustainedProbeCursor {
    resource_index: usize,
    window_offset: u64,
}

struct SustainedChildPlaylist {
    url: Url,
    role: String,
    cursor: SustainedProbeCursor,
    ranged: bool,
}

struct SustainedPlaylistSet {
    children: Vec<SustainedChildPlaylist>,
    next_child_index: usize,
}

impl SustainedPlaylistSet {
    fn new(urls: Vec<Url>) -> Self {
        Self {
            children: urls
                .into_iter()
                .map(|url| SustainedChildPlaylist {
                    url,
                    role: "unresolved_child".to_owned(),
                    cursor: SustainedProbeCursor::default(),
                    ranged: false,
                })
                .collect(),
            next_child_index: 0,
        }
    }

    fn len(&self) -> usize {
        self.children.len()
    }

    fn playlist_mut(&mut self, index: usize) -> &mut SustainedChildPlaylist {
        &mut self.children[index]
    }

    fn record_range(&mut self, index: usize) {
        self.children[index].ranged = true;
    }

    fn all_initial_children_ranged(&self) -> bool {
        !self.children.is_empty() && self.children.iter().all(|child| child.ranged)
    }

    fn next_index(&mut self) -> Option<usize> {
        if self.children.is_empty() {
            return None;
        }
        let index = self.next_child_index % self.children.len();
        self.next_child_index = (index + 1) % self.children.len();
        Some(index)
    }
}

fn next_sustained_probe_request(
    resources: &[HlsProbeResource],
    cursor: &mut SustainedProbeCursor,
) -> Option<SustainedProbeRequest> {
    let media_resources = resources
        .iter()
        .filter(|resource| !resource.initialization && !resource.uri.ends_with(".m3u8"))
        .collect::<Vec<_>>();
    if media_resources.is_empty() {
        return None;
    }

    cursor.resource_index %= media_resources.len();
    let resource = media_resources[cursor.resource_index];
    let (range, has_more_windows) = match &resource.range {
        Some(resource_range) => {
            let length = resource_range.end - resource_range.start + 1;
            if cursor.window_offset >= length {
                cursor.window_offset = 0;
            }
            let start = resource_range.start + cursor.window_offset;
            let end = resource_range
                .end
                .min(start.saturating_add(SUSTAINED_READ_LIMIT - 1));
            (ByteRangeRequest { start, end }, end < resource_range.end)
        }
        None => (
            ByteRangeRequest {
                start: 0,
                end: SUSTAINED_READ_LIMIT - 1,
            },
            false,
        ),
    };

    if has_more_windows {
        cursor.window_offset += SUSTAINED_READ_LIMIT;
    } else {
        cursor.resource_index = (cursor.resource_index + 1) % media_resources.len();
        cursor.window_offset = 0;
    }

    Some(SustainedProbeRequest {
        uri: resource.uri.clone(),
        range,
    })
}

fn hls_probe_resources(playlist: &str) -> Result<Vec<HlsProbeResource>, ()> {
    let mut resources = Vec::new();
    let mut pending_range = None;
    let mut range_end_by_uri = std::collections::HashMap::<String, u64>::new();

    for line in playlist
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line.starts_with("#EXT-X-BYTERANGE:") {
            pending_range = Some(parse_hls_byte_range(
                line.trim_start_matches("#EXT-X-BYTERANGE:"),
            )?);
            continue;
        }
        let (uri, range) = if line.starts_with("#EXT-X-MAP:") {
            let uri = hls_attribute_value(line, "URI").ok_or(())?;
            let range = hls_attribute_value(line, "BYTERANGE")
                .map(|value| parse_hls_byte_range(&value))
                .transpose()?;
            (uri, range)
        } else if !line.starts_with('#') {
            (line.to_owned(), pending_range.take())
        } else {
            continue;
        };

        let resolved_range = if let Some((length, offset)) = range {
            let start = offset
                .or_else(|| range_end_by_uri.get(&uri).copied())
                .unwrap_or(0);
            let end = start
                .checked_add(length.checked_sub(1).ok_or(())?)
                .ok_or(())?;
            range_end_by_uri.insert(uri.clone(), end.checked_add(1).ok_or(())?);
            Some(ByteRangeRequest { start, end })
        } else {
            None
        };
        resources.push(HlsProbeResource {
            uri,
            range: resolved_range,
            initialization: line.starts_with("#EXT-X-MAP:"),
        });
    }
    Ok(resources)
}

fn parse_hls_byte_range(value: &str) -> Result<(u64, Option<u64>), ()> {
    let (length, offset) = value
        .split_once('@')
        .map_or((value, None), |(length, offset)| (length, Some(offset)));
    let length = length.parse::<u64>().map_err(|_| ())?;
    if length == 0 {
        return Err(());
    }
    let offset = offset.map(str::parse::<u64>).transpose().map_err(|_| ())?;
    Ok((length, offset))
}

fn hls_attribute_value(line: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = line.find(&needle)? + needle.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_owned())
}

fn content_range_bounds(value: &str) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, _) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    (end >= start).then_some((start, end))
}

fn assert_playlist_stays_on_lan(
    case: &LiveCase,
    base_url: &Url,
    playlist: &str,
    media_url: &str,
    label: &str,
) -> Vec<Url> {
    let mut media_playlist_urls = Vec::new();
    for uri in playlist_referenced_uris(playlist) {
        let resolved = base_url.join(&uri).unwrap_or_else(|error| {
            panic!(
                "{}: {label} playlist contains an unresolvable URI: {error}",
                case.id
            )
        });
        assert_lan_media_url(case, resolved.as_str(), media_url, label);
        if resolved.path().ends_with(".m3u8") {
            media_playlist_urls.push(resolved);
        }
    }
    media_playlist_urls
}

fn playlist_referenced_uris(playlist: &str) -> Vec<String> {
    let mut uris = Vec::new();
    for line in playlist
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !line.starts_with('#') {
            uris.push(line.to_owned());
            continue;
        }

        let mut remainder = line;
        while let Some(attribute_start) = remainder.find("URI=\"") {
            let value_start = attribute_start + 5;
            let Some(value_end) = remainder[value_start..].find('"') else {
                break;
            };
            uris.push(remainder[value_start..value_start + value_end].to_owned());
            remainder = &remainder[value_start + value_end + 1..];
        }
    }
    uris
}

fn assert_lan_media_url(case: &LiveCase, uri: &str, media_url: &str, label: &str) -> Url {
    let parsed = Url::parse(uri)
        .unwrap_or_else(|error| panic!("{}: {label} URI is not absolute: {error}", case.id));
    let media = Url::parse(media_url)
        .unwrap_or_else(|error| panic!("{}: live media URL is invalid: {error}", case.id));
    assert_eq!(
        (
            media.scheme(),
            media.host_str(),
            media.port_or_known_default()
        ),
        (
            parsed.scheme(),
            parsed.host_str(),
            parsed.port_or_known_default()
        ),
        "{}: {label} escaped the LAN media listener: origin={}",
        case.id,
        lan_media_origin_for_diagnostic(&parsed)
    );
    parsed
}

fn assert_clean_lan_url(case: &LiveCase, url: &Url) {
    assert!(
        url.username().is_empty() && url.password().is_none(),
        "{}: completed LAN URI contained user information",
        case.id
    );
    assert!(
        url.query().is_none() && url.fragment().is_none(),
        "{}: completed LAN URI contained query or fragment data",
        case.id
    );
}

fn assert_playlist_references_are_clean(
    case: &LiveCase,
    base_url: &Url,
    playlist: &str,
    media_url: &str,
) {
    for reference in playlist_referenced_uris(playlist) {
        let url = base_url.join(&reference).unwrap_or_else(|_| {
            panic!(
                "{}: completed playlist reference could not be resolved",
                case.id
            )
        });
        assert_lan_media_url(
            case,
            url.as_str(),
            media_url,
            "completed playlist reference",
        );
        assert_clean_lan_url(case, &url);
    }
}

async fn read_completed_hls_payloads(
    case: &LiveCase,
    http: &reqwest::Client,
    source: &PlaybackSource,
    media_url: &str,
) -> OfflinePayload {
    let master_url = assert_lan_media_url(
        case,
        &source.uri,
        media_url,
        "completed cache-only master playlist",
    );
    assert_clean_lan_url(case, &master_url);
    let master = fetch_offline_playlist(case, http, &master_url, media_url).await;
    assert_playlist_references_are_clean(case, &master_url, &master, media_url);
    let child_urls = assert_playlist_stays_on_lan(
        case,
        &master_url,
        &master,
        media_url,
        "completed cache-only master playlist",
    );
    assert!(
        !child_urls.is_empty(),
        "{}: completed master had no selected audio/video playlists",
        case.id
    );
    assert!(
        child_urls.len() <= SUSTAINED_MAX_CHILD_PLAYLISTS,
        "{}: completed master exceeded its child-playlist limit",
        case.id
    );

    let mut resources = Vec::new();
    let mut seen = HashSet::new();
    for child_url in child_urls {
        assert_clean_lan_url(case, &child_url);
        let playlist = fetch_offline_playlist(case, http, &child_url, media_url).await;
        assert_playlist_references_are_clean(case, &child_url, &playlist, media_url);
        assert_playlist_stays_on_lan(
            case,
            &child_url,
            &playlist,
            media_url,
            "completed cache-only media playlist",
        );
        let parsed = hls_probe_resources(&playlist).unwrap_or_else(|_| {
            panic!(
                "{}: completed media playlist contained invalid or unsupported byte ranges",
                case.id
            )
        });
        for resource in parsed {
            let url = child_url.join(&resource.uri).unwrap_or_else(|_| {
                panic!(
                    "{}: completed media resource URI could not be resolved",
                    case.id
                )
            });
            assert_lan_media_url(
                case,
                url.as_str(),
                media_url,
                "completed cache-only media resource",
            );
            assert_clean_lan_url(case, &url);
            let key = format!("{}|{:?}", url.as_str(), resource.range);
            if seen.insert(key) {
                resources.push(HlsProbeResource {
                    uri: url.to_string(),
                    range: resource.range,
                    initialization: resource.initialization,
                });
                assert!(
                    resources.len() <= OFFLINE_MAX_RESOURCES,
                    "{}: completed HLS resource count exceeded its limit",
                    case.id
                );
            }
        }
    }
    assert!(
        resources.iter().any(|resource| !resource.initialization),
        "{}: completed media playlists advertised no media segments",
        case.id
    );

    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    for resource in &resources {
        if let Some(range) = &resource.range {
            let mut start = range.start;
            while start <= range.end {
                let end = range
                    .end
                    .min(start.saturating_add(SUSTAINED_READ_LIMIT - 1));
                read_offline_range(
                    case,
                    http,
                    &resource.uri,
                    ByteRangeRequest { start, end },
                    media_url,
                    &mut bytes,
                    &mut digest,
                )
                .await;
                start = end.saturating_add(1);
            }
        } else {
            read_offline_resource(
                case,
                http,
                &resource.uri,
                media_url,
                &mut bytes,
                &mut digest,
            )
            .await;
        }
    }
    OfflinePayload {
        resources,
        total_bytes: bytes,
        sha256: format!("{:x}", digest.finalize()),
    }
}

async fn fetch_offline_playlist(
    case: &LiveCase,
    http: &reqwest::Client,
    url: &Url,
    media_url: &str,
) -> String {
    assert_lan_media_url(case, url.as_str(), media_url, "cache-only playlist");
    let response = http
        .get(url.clone())
        .send()
        .await
        .unwrap_or_else(|_| panic!("{}: cache-only playlist request failed", case.id));
    assert_eq!(
        StatusCode::OK,
        response.status(),
        "{}: cache-only playlist was not available",
        case.id
    );
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .unwrap_or_else(|_| panic!("{}: cache-only playlist body failed", case.id))
    {
        if bytes.len().saturating_add(chunk.len()) > SUSTAINED_PLAYLIST_LIMIT {
            panic!("{}: cache-only playlist exceeded its 1 MiB limit", case.id);
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(bytes)
        .unwrap_or_else(|_| panic!("{}: cache-only playlist was not UTF-8", case.id));
    assert!(
        body.starts_with("#EXTM3U"),
        "{}: cache-only playlist was malformed",
        case.id
    );
    body
}

async fn read_offline_range(
    case: &LiveCase,
    http: &reqwest::Client,
    uri: &str,
    range: ByteRangeRequest,
    media_url: &str,
    aggregate_bytes: &mut u64,
    digest: &mut Sha256,
) {
    let url = assert_lan_media_url(case, uri, media_url, "cache-only byte range");
    assert_clean_lan_url(case, &url);
    let response = http
        .get(url)
        .header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", range.start, range.end),
        )
        .send()
        .await
        .unwrap_or_else(|_| panic!("{}: cache-only byte-range request failed", case.id));
    assert_eq!(
        StatusCode::PARTIAL_CONTENT,
        response.status(),
        "{}: cache-only range did not return partial content",
        case.id
    );
    let returned = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(content_range_bounds);
    assert_eq!(
        Some((range.start, range.end)),
        returned,
        "{}: cache-only Content-Range differed",
        case.id
    );
    let expected = range.end - range.start + 1;
    let received = consume_bounded_body(case, response, expected, aggregate_bytes, digest).await;
    assert_eq!(
        expected, received,
        "{}: cache-only range returned an incomplete body",
        case.id
    );
}

async fn read_offline_resource(
    case: &LiveCase,
    http: &reqwest::Client,
    uri: &str,
    media_url: &str,
    aggregate_bytes: &mut u64,
    digest: &mut Sha256,
) {
    let url = assert_lan_media_url(case, uri, media_url, "cache-only segment");
    assert_clean_lan_url(case, &url);
    let response = http
        .get(url)
        .send()
        .await
        .unwrap_or_else(|_| panic!("{}: cache-only segment request failed", case.id));
    assert_eq!(
        StatusCode::OK,
        response.status(),
        "{}: cache-only segment was not fully available",
        case.id
    );
    consume_bounded_body(
        case,
        response,
        OFFLINE_MAX_VERIFIED_BYTES,
        aggregate_bytes,
        digest,
    )
    .await;
}

async fn consume_bounded_body(
    case: &LiveCase,
    mut response: reqwest::Response,
    expected_max: u64,
    aggregate_bytes: &mut u64,
    digest: &mut Sha256,
) -> u64 {
    let mut received = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .unwrap_or_else(|_| panic!("{}: cache-only response body failed", case.id))
    {
        let chunk_len = u64::try_from(chunk.len()).expect("chunk length should fit u64");
        received = bounded_byte_sum(received, chunk_len)
            .unwrap_or_else(|| panic!("{}: response byte count overflowed", case.id));
        *aggregate_bytes = bounded_byte_sum(*aggregate_bytes, chunk_len)
            .unwrap_or_else(|| panic!("{}: aggregate byte count overflowed", case.id));
        assert!(
            received <= expected_max && *aggregate_bytes <= OFFLINE_MAX_VERIFIED_BYTES,
            "{}: cache-only byte validation exceeded its 1 GiB ceiling",
            case.id
        );
        digest.update(&chunk);
    }
    received
}

fn bounded_byte_sum(current: u64, added: u64) -> Option<u64> {
    let total = current.checked_add(added)?;
    (total <= OFFLINE_MAX_VERIFIED_BYTES).then_some(total)
}

fn cached_media_checksums(
    root: &Path,
    session_id: &str,
    resources: &[HlsProbeResource],
) -> Result<std::collections::BTreeMap<String, (u64, String)>, ()> {
    use std::os::unix::fs::MetadataExt;

    let session_dir = root.join(".tvos-net-player/hls").join(session_id);
    let directory_metadata = fs::symlink_metadata(&session_dir).map_err(|_| ())?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(());
    }
    let mut checksums = std::collections::BTreeMap::new();
    let mut unique_ids = HashSet::new();
    let mut aggregate_length = 0_u64;
    for resource in resources {
        let url = Url::parse(&resource.uri).map_err(|_| ())?;
        let id = url.path_segments().and_then(Iterator::last).ok_or(())?;
        if id.is_empty()
            || id == "."
            || id == ".."
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(());
        }
        if !unique_ids.insert(id.to_owned()) {
            continue;
        }
        let path = session_dir.join(id);
        let entry_metadata = fs::symlink_metadata(&path).map_err(|_| ())?;
        if entry_metadata.file_type().is_symlink() || !entry_metadata.is_file() {
            return Err(());
        }
        aggregate_length = aggregate_length
            .checked_add(entry_metadata.len())
            .ok_or(())?;
        if aggregate_length > OFFLINE_MAX_VERIFIED_BYTES {
            return Err(());
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| ())?;
        let before = file.metadata().map_err(|_| ())?;
        let identity = (before.dev(), before.ino(), before.len());
        if !before.is_file() || before.len() != entry_metadata.len() {
            return Err(());
        }
        let mut digest = Sha256::new();
        let mut bytes_read = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).map_err(|_| ())?;
            if count == 0 {
                break;
            }
            bytes_read = bytes_read.checked_add(count as u64).ok_or(())?;
            if bytes_read > before.len() {
                return Err(());
            }
            digest.update(&buffer[..count]);
        }
        let after = file.metadata().map_err(|_| ())?;
        if (after.dev(), after.ino(), after.len()) != identity || bytes_read != before.len() {
            return Err(());
        }
        checksums.insert(
            id.to_owned(),
            (bytes_read, format!("{:x}", digest.finalize())),
        );
    }
    if checksums.is_empty() {
        return Err(());
    }
    Ok(checksums)
}

async fn sustain_offline_cache_reads(
    case: &LiveCase,
    http: &reqwest::Client,
    resources: &[HlsProbeResource],
    media_url: &str,
    duration: Duration,
    already_read: u64,
) -> u64 {
    let deadline = tokio::time::Instant::now() + duration;
    let mut cursor = SustainedProbeCursor::default();
    let mut total_bytes = already_read;
    let mut digest = Sha256::new();
    let mut requests = 0_u64;
    while tokio::time::Instant::now() < deadline {
        let request = next_sustained_probe_request(resources, &mut cursor).unwrap_or_else(|| {
            panic!(
                "{}: no media byte ranges available for offline reads",
                case.id
            )
        });
        let read = tokio::time::timeout_at(
            deadline,
            read_offline_range(
                case,
                http,
                &request.uri,
                request.range,
                media_url,
                &mut total_bytes,
                &mut digest,
            ),
        )
        .await;
        if read.is_err() && tokio::time::Instant::now() >= deadline {
            break;
        }
        read.unwrap_or_else(|_| {
            panic!(
                "{}: offline cache-read timed out before its deadline",
                case.id
            )
        });
        requests = requests.saturating_add(1);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining > Duration::from_millis(250) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        } else if !remaining.is_zero() {
            tokio::time::sleep_until(deadline).await;
        }
    }
    assert!(
        requests > 0,
        "{}: 300-second offline range loop issued no requests",
        case.id
    );
    total_bytes
}

#[derive(Serialize)]
struct LanPlaybackReady<'a> {
    phase: &'a str,
    uri: String,
    case_id: String,
    task_id: String,
    total_bytes: u64,
    expires_in_seconds: u64,
}

fn lan_media_origin_for_diagnostic(url: &Url) -> String {
    url.origin().ascii_serialization()
}

#[derive(Debug, Deserialize)]
struct LiveFixtureSet {
    cases: Vec<LiveCase>,
}

impl LiveFixtureSet {
    fn load() -> Self {
        let path = env::var_os("BILIBILI_LIVE_E2E_FIXTURE")
            .map(PathBuf::from)
            .unwrap_or_else(default_fixture_path);
        Self::load_from_path(path)
    }

    fn load_from_path(path: PathBuf) -> Self {
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
    }
}

#[derive(Debug, Deserialize)]
struct LiveCase {
    id: String,
    url: String,
    #[serde(default)]
    url_env: Option<String>,
    expected_source_kind: String,
    #[serde(default)]
    expected_candidate_source_kind: Option<String>,
    minimum_candidates: usize,
    selection: SelectionPolicy,
    #[serde(default)]
    requires_restricted_area_path: bool,
    #[serde(default)]
    requires_authentication: bool,
    #[serde(default)]
    requires_collection_list_validation: bool,
    #[serde(default)]
    requires_stable_item_selection: bool,
    #[serde(default)]
    requires_live_sample_override: bool,
    playback_options: LivePlaybackOptions,
    timeout_seconds: Option<u64>,
}

impl LiveCase {
    fn source(&self) -> String {
        self.url_env
            .as_deref()
            .and_then(|env_key| env::var(env_key).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| self.url.clone())
    }

    fn has_source_override(&self) -> bool {
        self.url_env
            .as_deref()
            .and_then(|env_key| env::var(env_key).ok())
            .is_some_and(|value| !value.trim().is_empty())
    }

    fn selection_request(
        &self,
        candidates: &[BilibiliResolutionCandidate],
        default_candidate_token: &str,
    ) -> LiveSelectionRequest {
        match self.selection {
            SelectionPolicy::DefaultOrFirst => {
                let candidate_token = if default_candidate_token.trim().is_empty() {
                    first_candidate_token(self, candidates)
                } else {
                    default_candidate_token.to_owned()
                };
                LiveSelectionRequest::single(candidate_token)
            }
            SelectionPolicy::First => {
                LiveSelectionRequest::single(first_candidate_token(self, candidates))
            }
            SelectionPolicy::MultipleFirstTwo => {
                LiveSelectionRequest::multiple(first_candidate_tokens(self, candidates, 2))
            }
            SelectionPolicy::RangeFirstTwo => {
                let candidates = first_candidates(self, candidates, 2);
                LiveSelectionRequest::range(candidates[0], candidates[1], 2)
            }
            SelectionPolicy::All => LiveSelectionRequest::all(candidates.len()),
        }
    }
}

struct LiveSelectionRequest {
    selection: BilibiliResolutionSelection,
    expected_result_items: usize,
    expected_playable_results: usize,
}

impl LiveSelectionRequest {
    fn single(candidate_token: String) -> Self {
        Self {
            selection: BilibiliResolutionSelection {
                mode: BilibiliResolutionSelectionMode::Single.into(),
                candidate_tokens: vec![candidate_token],
                ..Default::default()
            },
            expected_result_items: 1,
            expected_playable_results: 1,
        }
    }

    fn multiple(candidate_tokens: Vec<String>) -> Self {
        let expected_playable_results = candidate_tokens.len();
        Self {
            selection: BilibiliResolutionSelection {
                mode: BilibiliResolutionSelectionMode::Multiple.into(),
                candidate_tokens,
                ..Default::default()
            },
            expected_result_items: expected_playable_results,
            expected_playable_results,
        }
    }

    fn range(
        start: &BilibiliResolutionCandidate,
        end: &BilibiliResolutionCandidate,
        expected_playable_results: usize,
    ) -> Self {
        Self {
            selection: BilibiliResolutionSelection {
                mode: BilibiliResolutionSelectionMode::Range.into(),
                range_start_candidate_token: start.candidate_token.clone(),
                range_end_candidate_token: end.candidate_token.clone(),
                ..Default::default()
            },
            expected_result_items: expected_playable_results,
            expected_playable_results,
        }
    }

    fn all(expected_playable_results: usize) -> Self {
        Self {
            selection: BilibiliResolutionSelection {
                mode: BilibiliResolutionSelectionMode::All.into(),
                ..Default::default()
            },
            expected_result_items: expected_playable_results,
            expected_playable_results,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SelectionPolicy {
    DefaultOrFirst,
    First,
    MultipleFirstTwo,
    RangeFirstTwo,
    All,
}

#[derive(Debug, Deserialize)]
struct LivePlaybackOptions {
    quality_preference: String,
    encoding_preference: String,
    prefer_tv_api: bool,
    #[serde(default)]
    audio_language: String,
}

impl LivePlaybackOptions {
    fn to_proto(&self) -> BilibiliPlaybackOptions {
        BilibiliPlaybackOptions {
            quality_preference: self.quality_preference.clone(),
            encoding_preference: self.encoding_preference.clone(),
            prefer_tv_api: self.prefer_tv_api,
            audio_language: self.audio_language.clone(),
            playback_policy: None,
        }
    }

    fn to_playback_spec(&self) -> BilibiliPlaybackSpec {
        BilibiliPlaybackSpec {
            quality_qn: match self.quality_preference.as_str() {
                "360p" => 16,
                "480p" => 32,
                "720p" => 64,
                "720p60" => 74,
                "1080p" => 80,
                "1080p+" => 112,
                _ => 0,
            },
            codec: match self.encoding_preference.as_str() {
                "h264" => BilibiliVideoCodec::H264.into(),
                "hevc" => BilibiliVideoCodec::Hevc.into(),
                "av1" => BilibiliVideoCodec::Av1.into(),
                _ => BilibiliVideoCodec::Auto.into(),
            },
            audio_language: self.audio_language.clone(),
            policy: None,
        }
    }
}

fn first_candidate_token(case: &LiveCase, candidates: &[BilibiliResolutionCandidate]) -> String {
    candidates
        .first()
        .unwrap_or_else(|| panic!("{}: resolved no selectable candidates", case.id))
        .candidate_token
        .clone()
}

fn first_candidate_tokens(
    case: &LiveCase,
    candidates: &[BilibiliResolutionCandidate],
    count: usize,
) -> Vec<String> {
    first_candidates(case, candidates, count)
        .into_iter()
        .map(|candidate| candidate.candidate_token.clone())
        .collect()
}

fn first_candidates<'a>(
    case: &LiveCase,
    candidates: &'a [BilibiliResolutionCandidate],
    count: usize,
) -> Vec<&'a BilibiliResolutionCandidate> {
    assert!(
        candidates.len() >= count,
        "{}: expected at least {} candidates, got {}",
        case.id,
        count,
        candidates.len()
    );
    candidates.iter().take(count).collect()
}

fn assert_resolution_candidate_contract(
    case: &LiveCase,
    candidates: &[BilibiliResolutionCandidate],
) {
    if let Some(expected_source_kind) = case.expected_candidate_source_kind.as_deref() {
        for candidate in candidates {
            assert_eq!(
                expected_source_kind, candidate.source_kind,
                "{}: candidate has unexpected source kind",
                case.id
            );
        }
    }

    if case.requires_stable_item_selection {
        for candidate in candidates {
            assert_stable_item_candidate(case, candidate);
        }
    }
}

fn assert_stable_item_candidate(case: &LiveCase, candidate: &BilibiliResolutionCandidate) {
    assert!(
        (1..=100).contains(&candidate.index),
        "{}: stable collection item index is outside the bounded candidate window",
        case.id
    );
    let identity = candidate.identity.as_ref().unwrap_or_else(|| {
        panic!(
            "{}: stable collection item candidate omitted content identity",
            case.id
        )
    });
    assert_eq!(
        BilibiliContentKind::CollectionItem,
        identity.kind(),
        "{}: candidate identity is not a collection item",
        case.id
    );
    assert!(
        identity.cid > 0,
        "{}: stable collection item candidate omitted cid",
        case.id
    );
    assert!(
        identity.aid > 0 || !identity.bvid.trim().is_empty(),
        "{}: stable collection item candidate omitted video identity",
        case.id
    );
}

fn default_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".agents/skills/bilibili-live-e2e/references/live-cases.json")
}

#[derive(Debug, Eq, PartialEq)]
enum LiveRunDecision {
    Run,
    Skip(&'static str),
}

#[derive(Debug, Default)]
struct LiveRunPolicy {
    filter: Option<HashSet<String>>,
    include_authenticated: bool,
    include_collection_list: bool,
    sustained_duration: Option<Duration>,
    full_fill: bool,
    offline_duration: Option<Duration>,
    ready_file: Option<PathBuf>,
}

struct SustainedProbeContext {
    client: reqwest::Client,
    duration: Duration,
}

#[derive(Clone)]
struct FullFillContext {
    offline_duration: Option<Duration>,
    ready_file: Option<PathBuf>,
}

struct OfflinePayload {
    resources: Vec<HlsProbeResource>,
    total_bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RangeExtentCheckpoint {
    start: u64,
    end: u64,
    sha256: String,
}

#[derive(Clone, Debug, Default)]
struct RangeCheckpointSnapshot {
    manifests: std::collections::BTreeMap<String, Vec<RangeExtentCheckpoint>>,
    durable_bytes: u64,
}

struct QuiescedRestartCheckpoint {
    fill_status:
        Option<tvos_net_player_cache_server::generated::tvos_net_player::v1::HlsCacheFillStatus>,
    range_checkpoints: RangeCheckpointSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuiescedCheckpointEvidence {
    Partial {
        status_bytes: u64,
        durable_bytes: u64,
        extent_count: usize,
    },
    Completed,
    Inconclusive,
}

impl LiveRunPolicy {
    fn from_env() -> Self {
        let full_fill = env_flag("BILIBILI_LIVE_E2E_FULL_FILL");
        let offline_duration = offline_duration_from_env();
        let ready_file = ready_file_from_env();
        assert!(
            offline_duration.is_none() || full_fill,
            "BILIBILI_LIVE_E2E_OFFLINE_SECONDS requires BILIBILI_LIVE_E2E_FULL_FILL=1"
        );
        assert!(
            ready_file.is_none() || (full_fill && offline_duration == Some(OFFLINE_READ_DURATION)),
            "BILIBILI_LIVE_E2E_LAN_PLAYBACK_READY_PATH requires full-fill mode and BILIBILI_LIVE_E2E_OFFLINE_SECONDS=300"
        );
        Self {
            filter: case_filter_from_env(),
            include_authenticated: env_flag("BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED"),
            include_collection_list: env_flag("BILIBILI_LIVE_E2E_INCLUDE_COLLECTION_LIST"),
            sustained_duration: sustained_duration_from_env(),
            full_fill,
            offline_duration,
            ready_file,
        }
    }

    fn run_decision(&self, case: &LiveCase) -> LiveRunDecision {
        if self.full_fill && !supports_full_fill_case(case) {
            return LiveRunDecision::Skip(
                "full-fill mode requires an allowlisted canonical single-result case",
            );
        }
        if self.filter.is_none() && case.requires_restricted_area_path {
            return LiveRunDecision::Skip("requires explicit restricted-area live validation");
        }
        if self.filter.is_none()
            && case.requires_collection_list_validation
            && !self.include_collection_list
        {
            return LiveRunDecision::Skip("requires explicit collection/list live validation");
        }
        if self.filter.is_none() && case.requires_authentication && !self.include_authenticated {
            return LiveRunDecision::Skip("requires authenticated live validation");
        }
        if self.filter.is_none()
            && case.requires_live_sample_override
            && !case.has_source_override()
        {
            return LiveRunDecision::Skip("requires live sample URL override");
        }
        LiveRunDecision::Run
    }
}

fn supports_full_fill_case(case: &LiveCase) -> bool {
    matches!(
        (case.id.as_str(), case.selection),
        ("ordinary-video-playlist", SelectionPolicy::First)
            | ("bangumi-media-series", SelectionPolicy::First)
            | (
                "bangumi-episode",
                SelectionPolicy::DefaultOrFirst | SelectionPolicy::First
            )
    )
}

fn ready_file_from_env() -> Option<PathBuf> {
    let configured = env::var_os("BILIBILI_LIVE_E2E_LAN_PLAYBACK_READY_PATH")?;
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root should resolve");
    let expected = repo_root.join(".codex-tmp/fill-pr4-live/ready.json");
    let configured = PathBuf::from(configured);
    assert_eq!(
        expected, configured,
        "BILIBILI_LIVE_E2E_LAN_PLAYBACK_READY_PATH must target the lead-owned ignored ready file"
    );
    Some(expected)
}

fn offline_duration_from_env() -> Option<Duration> {
    let value = env::var("BILIBILI_LIVE_E2E_OFFLINE_SECONDS").ok();
    parse_offline_duration(value.as_deref())
        .unwrap_or_else(|message| panic!("BILIBILI_LIVE_E2E_OFFLINE_SECONDS {message}"))
}

fn parse_offline_duration(value: Option<&str>) -> Result<Option<Duration>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "must be an integer from 180 through 300 when set")?;
    if !(180..=300).contains(&seconds) {
        return Err("must be between 180 and 300 seconds when set");
    }
    Ok(Some(Duration::from_secs(seconds)))
}

fn sustained_duration_from_env() -> Option<Duration> {
    let value = env::var("BILIBILI_LIVE_E2E_SUSTAINED_SECONDS").ok();
    parse_sustained_duration(value.as_deref())
        .unwrap_or_else(|message| panic!("BILIBILI_LIVE_E2E_SUSTAINED_SECONDS {message}"))
}

fn parse_sustained_duration(value: Option<&str>) -> Result<Option<Duration>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "must be a positive integer when set")?;
    if seconds == 0 {
        return Err("must be greater than zero when set");
    }
    Ok(Some(Duration::from_secs(seconds)))
}

fn case_filter_from_env() -> Option<HashSet<String>> {
    env::var("BILIBILI_LIVE_E2E_CASES")
        .ok()
        .map(parse_case_filter)
        .filter(|values| !values.is_empty())
}

fn parse_case_filter(value: String) -> HashSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<HashSet<_>>()
}

fn env_flag(key: &str) -> bool {
    env::var(key).ok().as_deref().is_some_and(parse_env_flag)
}

fn parse_env_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

async fn fetch_bilibili_credential_status(
    channel: tonic::transport::Channel,
) -> BilibiliCredentialStatus {
    ServerServiceClient::new(channel)
        .get_bilibili_credential_status(Request::new(GetBilibiliCredentialStatusRequest {}))
        .await
        .unwrap_or_else(|_| panic!("failed to read Bilibili credential status from live server"))
        .into_inner()
}

fn assert_authenticated_case_ready(case: &LiveCase, status: Option<&BilibiliCredentialStatus>) {
    if !case.requires_authentication {
        return;
    }
    let Some(status) = status else {
        panic!(
            "{}: credential failure: credential status was not fetched",
            case.id
        );
    };
    if status.credential_file_loaded && status.web_cookie_present {
        return;
    }

    panic!(
        "{}: credential failure: authenticated case requires a loaded BBDown credential file with a web cookie; {}",
        case.id,
        credential_status_summary(status)
    );
}

fn credential_status_summary(status: &BilibiliCredentialStatus) -> String {
    format!(
        "state={} credential_file_loaded={} web_cookie_present={} access_key_present={} tv_access_key_present={}",
        status.state,
        status.credential_file_loaded,
        status.web_cookie_present,
        status.access_key_present,
        status.tv_access_key_present
    )
}

fn live_failure_message(
    case: &LiveCase,
    phase: &str,
    detail: &str,
    credential_status: Option<&BilibiliCredentialStatus>,
) -> String {
    let class = classify_live_failure(case, phase, detail, credential_status);
    let detail = safe_live_failure_detail(detail, credential_status);
    format!("{}: {phase} failed [{}]: {detail}", case.id, class.as_str())
}

fn live_task_failure_message(
    case: &LiveCase,
    phase: &str,
    task: &Task,
    credential_status: Option<&BilibiliCredentialStatus>,
) -> String {
    let classification_detail = task_failure_classification_detail(task);
    let class = classify_live_failure(case, phase, &classification_detail, credential_status);
    let failed_result_count = task
        .result_items
        .iter()
        .filter(|item| item.state() == TaskState::Failed)
        .count();
    let detail = safe_live_failure_detail(&task.message, credential_status);
    format!(
        "{}: {phase} failed [{}]: {detail}; failed_result_count={failed_result_count}",
        case.id,
        class.as_str(),
    )
}

fn safe_live_failure_detail<'a>(
    detail: &'a str,
    credential_status: Option<&BilibiliCredentialStatus>,
) -> &'a str {
    if credential_status
        .is_some_and(|status| status.credential_path_configured || status.credential_file_loaded)
    {
        "upstream detail omitted because credential material is configured"
    } else {
        detail
    }
}

fn task_failure_classification_detail(task: &Task) -> String {
    let mut details = Vec::with_capacity(task.result_items.len() + 1);
    details.push(task.message.as_str());
    details.extend(
        task.result_items
            .iter()
            .map(|item| item.message.as_str())
            .filter(|message| !message.trim().is_empty()),
    );
    details.join("; ")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveFailureClass {
    Credential,
    EmptyAccountState,
    UpstreamSchemaOrAvailability,
    RestrictedProxy,
    ServerBug,
}

impl LiveFailureClass {
    fn as_str(self) -> &'static str {
        match self {
            LiveFailureClass::Credential => "credential",
            LiveFailureClass::EmptyAccountState => "empty_account_state",
            LiveFailureClass::UpstreamSchemaOrAvailability => "upstream_schema_or_availability",
            LiveFailureClass::RestrictedProxy => "restricted_proxy",
            LiveFailureClass::ServerBug => "server_bug",
        }
    }
}

fn classify_live_failure(
    case: &LiveCase,
    phase: &str,
    detail: &str,
    credential_status: Option<&BilibiliCredentialStatus>,
) -> LiveFailureClass {
    if case.requires_authentication
        && credential_status
            .is_some_and(|status| !status.credential_file_loaded || !status.web_cookie_present)
    {
        return LiveFailureClass::Credential;
    }
    if let Some(class) = tagged_live_failure_class(detail) {
        return class;
    }

    let detail = untagged_live_failure_detail(detail);
    if case.requires_authentication
        && contains_any(
            &detail,
            &[
                "credential",
                "cookie",
                "login",
                "not logged",
                "sessdata",
                "csrf",
                "unauthorized",
                "-101",
                "账号未登录",
                "未登录",
            ],
        )
    {
        return LiveFailureClass::Credential;
    }
    if contains_any(
        &detail,
        &[
            "selected bilibili item",
            "selected collection item",
            "was not found",
            "no longer matches",
        ],
    ) {
        return LiveFailureClass::UpstreamSchemaOrAvailability;
    }
    if case.requires_authentication
        && contains_any(
            &detail,
            &[
                "empty",
                "no selected",
                "no selectable",
                "0 candidates",
                "got 0",
                "没有更多",
            ],
        )
    {
        return LiveFailureClass::EmptyAccountState;
    }
    if case.requires_restricted_area_path
        && contains_any(
            &detail,
            &[
                "area",
                "region",
                "restricted",
                "proxy",
                "地区",
                "版权",
                "不可观看",
            ],
        )
    {
        return LiveFailureClass::RestrictedProxy;
    }
    if phase.contains("resolve") || looks_like_upstream_planning_failure(&detail) {
        return LiveFailureClass::UpstreamSchemaOrAvailability;
    }
    LiveFailureClass::ServerBug
}

fn untagged_live_failure_detail(detail: &str) -> String {
    let mut detail = detail.to_ascii_lowercase();
    for class in [
        LiveFailureClass::Credential,
        LiveFailureClass::EmptyAccountState,
        LiveFailureClass::RestrictedProxy,
        LiveFailureClass::UpstreamSchemaOrAvailability,
        LiveFailureClass::ServerBug,
    ] {
        detail = detail.replace(
            &format!("[{BILIBILI_FAILURE_CLASS_TAG}={}]", class.as_str()),
            "",
        );
    }
    detail
}

fn tagged_live_failure_class(detail: &str) -> Option<LiveFailureClass> {
    if !detail.contains(CREDENTIAL_SAFE_CLIENT_DETAIL) {
        return None;
    }
    [
        LiveFailureClass::Credential,
        LiveFailureClass::EmptyAccountState,
        LiveFailureClass::RestrictedProxy,
        LiveFailureClass::UpstreamSchemaOrAvailability,
        LiveFailureClass::ServerBug,
    ]
    .into_iter()
    .find(|class| {
        detail.contains(&format!(
            "[{BILIBILI_FAILURE_CLASS_TAG}={}]",
            class.as_str()
        ))
    })
}

fn looks_like_upstream_planning_failure(detail: &str) -> bool {
    contains_any(
        detail,
        &[
            "upstream",
            "schema",
            "availability",
            "playurl",
            "resolve",
            "failed to fetch",
            "request failed",
            "network",
            "connection",
            "timed out",
            "timeout",
            "temporarily unavailable",
            "http status",
            "status 429",
            "status 500",
            "status 502",
            "status 503",
            "status 504",
            "missing field",
        ],
    )
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

#[derive(Clone, Default)]
struct LiveTaskTracker {
    task_id: Arc<Mutex<Option<String>>>,
}

impl LiveTaskTracker {
    fn record(&self, task_id: &str) {
        *self
            .task_id
            .lock()
            .expect("live task tracker lock should not be poisoned") = Some(task_id.to_owned());
    }

    fn task_id(&self) -> Option<String> {
        self.task_id
            .lock()
            .expect("live task tracker lock should not be poisoned")
            .clone()
    }
}

struct LiveTestServer {
    temp_root: Option<TempDir>,
    state: AppState,
    grpc_url: String,
    media_url: String,
    grpc_addr: std::net::SocketAddr,
    media_addr: std::net::SocketAddr,
    grpc_task: Option<JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
    media_task: Option<JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
    cache_max_bytes: u64,
    ready_file: Option<(PathBuf, Vec<u8>)>,
    restart_recovery_failure: Option<String>,
}

impl LiveTestServer {
    async fn start() -> Self {
        Self::start_with_fill_mode(false).await
    }

    async fn start_with_fill_mode(full_fill: bool) -> Self {
        let cache_max_bytes = live_cache_budget(full_fill);
        let temp_root = tempfile::tempdir().unwrap();
        let root_path = temp_root.path().canonicalize().unwrap();
        let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_addr = grpc_listener.local_addr().unwrap();
        let grpc_url = format!("http://{grpc_addr}");
        let media_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let media_addr = media_listener.local_addr().unwrap();
        let media_url = format!("http://{media_addr}");
        let options = live_server_options(
            root_path.clone(),
            grpc_url.clone(),
            media_url.clone(),
            cache_max_bytes,
        );
        let state = AppState::new(options);

        let grpc_task = tokio::spawn(run_grpc_listener(grpc_listener, state.clone()));
        let media_task = tokio::spawn(run_media_listener(media_listener, state.clone()));

        wait_for_grpc(&grpc_url).await;
        Self {
            temp_root: Some(temp_root),
            state,
            grpc_url,
            media_url,
            grpc_addr,
            media_addr,
            grpc_task: Some(grpc_task),
            media_task: Some(media_task),
            cache_max_bytes,
            ready_file: None,
            restart_recovery_failure: None,
        }
    }

    async fn channel(&self) -> tonic::transport::Channel {
        tonic::transport::Channel::from_shared(self.grpc_url.clone())
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    async fn shutdown(mut self, task_tracker: &LiveTaskTracker) -> Result<(), String> {
        let listener_result = self.stop_listeners().await;
        let ready_result = if listener_result.is_ok() {
            self.remove_ready_file()
        } else {
            Err("ready file retained because listener shutdown was not confirmed".to_owned())
        };
        let background_result = self.cancel_case_tasks_and_wait(task_tracker).await;
        let history_result = tokio::time::timeout(
            LIVE_CASE_TEARDOWN_TIMEOUT,
            self.state.shutdown_cdn_history_writer(),
        )
        .await
        .map_err(|_| "CDN history persistence did not become idle".to_owned());
        self.finish_teardown(combine_teardown_results(
            combine_teardown_results(listener_result, background_result),
            combine_teardown_results(history_result, ready_result),
        ))
    }

    async fn restart_preserving_state(&mut self) -> Result<(), String> {
        if let Err(error) = self.quiesce_for_restart().await {
            self.restart_recovery_failure = Some(error.clone());
            return Err(error);
        }
        if let Err(error) = self.restore_after_restart().await {
            self.restart_recovery_failure = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }

    async fn restart_preserving_state_with_checkpoint(
        &mut self,
        task_id: &str,
    ) -> Result<QuiescedRestartCheckpoint, String> {
        if let Err(error) = self.quiesce_for_restart().await {
            self.restart_recovery_failure = Some(error.clone());
            return Err(error);
        }
        let fill_status = match self.state.tasks.get_task(task_id) {
            Ok(task) => task.hls_cache_fill_status,
            Err(_) => {
                let error = "task status could not be read after fill quiescence".to_owned();
                self.restart_recovery_failure = Some(error.clone());
                return Err(error);
            }
        };
        let range_checkpoints = match snapshot_range_checkpoints(self.temp_root_path()) {
            Ok(snapshot) => snapshot,
            Err(()) => {
                let error =
                    "durable range checkpoints could not be inspected after quiescence".to_owned();
                self.restart_recovery_failure = Some(error.clone());
                return Err(error);
            }
        };
        if let Err(error) = self.restore_after_restart().await {
            self.restart_recovery_failure = Some(error.clone());
            return Err(error);
        }
        Ok(QuiescedRestartCheckpoint {
            fill_status,
            range_checkpoints,
        })
    }

    async fn quiesce_for_restart(&mut self) -> Result<(), String> {
        self.stop_listeners().await?;
        tokio::time::timeout(
            LIVE_CASE_TEARDOWN_TIMEOUT,
            self.state.shutdown_hls_fill_worker(),
        )
        .await
        .map_err(|_| "HLS fill worker did not join before same-root restart".to_owned())?;
        tokio::time::timeout(
            LIVE_CASE_TEARDOWN_TIMEOUT,
            self.state.shutdown_cdn_history_writer(),
        )
        .await
        .map_err(|_| "CDN history writer did not join before same-root restart".to_owned())?;
        if !self.state.background_work_is_idle() {
            return Err(format!(
                "background work was not idle after graceful restart shutdown ({})",
                self.state.background_work_diagnostics()
            ));
        }
        Ok(())
    }

    async fn restore_after_restart(&mut self) -> Result<(), String> {
        let root = self
            .temp_root
            .as_ref()
            .ok_or_else(|| "live e2e root was unavailable during restart".to_owned())?
            .path()
            .canonicalize()
            .map_err(|_| "live e2e root could not be revalidated during restart".to_owned())?;
        self.state = AppState::new(live_server_options(
            root,
            self.grpc_url.clone(),
            self.media_url.clone(),
            self.cache_max_bytes,
        ));
        let grpc_listener = TcpListener::bind(self.grpc_addr)
            .await
            .map_err(|_| "gRPC listener could not restart on its original address".to_owned())?;
        let media_listener = TcpListener::bind(self.media_addr)
            .await
            .map_err(|_| "media listener could not restart on its original address".to_owned())?;
        self.grpc_task = Some(tokio::spawn(run_grpc_listener(
            grpc_listener,
            self.state.clone(),
        )));
        self.media_task = Some(tokio::spawn(run_media_listener(
            media_listener,
            self.state.clone(),
        )));
        wait_for_grpc(&self.grpc_url).await;
        Ok(())
    }

    fn publish_ready_file(&mut self, path: PathBuf, content: Vec<u8>) -> Result<(), String> {
        if content.len() > READY_FILE_LIMIT {
            return Err("ready document exceeded its 4 KiB limit".to_owned());
        }
        let parent = path
            .parent()
            .ok_or_else(|| "ready file parent path was missing".to_owned())?;
        reject_symlink_components(parent)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => return Err("ready file already exists and was not overwritten".to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("ready file destination could not be checked".to_owned()),
        }
        fs::create_dir_all(parent)
            .map_err(|_| "ready file directory could not be created".to_owned())?;
        let temporary = parent.join(format!(".ready.json.{}.tmp", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| "ready file staging file could not be created".to_owned())?;
        let write_result = file
            .write_all(&content)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::hard_link(&temporary, &path))
            .and_then(|()| fs::remove_file(&temporary));
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err("ready file could not be atomically published".to_owned());
        }
        self.ready_file = Some((path, content));
        Ok(())
    }

    fn remove_ready_file(&mut self) -> Result<(), String> {
        let Some((path, expected)) = self.ready_file.take() else {
            return Ok(());
        };
        match fs::read(&path) {
            Ok(actual) if actual == expected => fs::remove_file(path)
                .map_err(|_| "ready file could not be removed after listener shutdown".to_owned()),
            Ok(_) => Err("ready file changed externally and was retained".to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("ready file could not be revalidated for cleanup".to_owned()),
        }
    }

    fn temp_root_path(&self) -> &Path {
        self.temp_root
            .as_ref()
            .expect("live e2e temp root should be present")
            .path()
    }

    fn finish_teardown(mut self, result: Result<(), String>) -> Result<(), String> {
        let result = match self.restart_recovery_failure.take() {
            Some(failure) => combine_teardown_results(
                result,
                Err(format!(
                    "restart did not reach a recoverable state: {failure}"
                )),
            ),
            None => result,
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let retained_root = self
                    .temp_root
                    .take()
                    .expect("live e2e temp root should be present")
                    .keep();
                Err(format!(
                    "{error}; retained live e2e root for recovery: {}",
                    retained_root.display()
                ))
            }
        }
    }

    async fn cancel_case_tasks_and_wait(
        &self,
        task_tracker: &LiveTaskTracker,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + LIVE_CASE_TEARDOWN_TIMEOUT;
        loop {
            let task_ids = self.case_task_ids(task_tracker)?;
            self.cancel_case_tasks(&task_ids)?;
            if self.case_tasks_are_terminal(&task_ids)? && self.state.background_work_is_idle() {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let stable_task_ids = self.case_task_ids(task_tracker)?;
                self.cancel_case_tasks(&stable_task_ids)?;
                if stable_task_ids == task_ids
                    && self.case_tasks_are_terminal(&stable_task_ids)?
                    && self.state.background_work_is_idle()
                {
                    tokio::time::timeout(
                        LIVE_CASE_TEARDOWN_TIMEOUT,
                        self.state.shutdown_hls_fill_worker(),
                    )
                    .await
                    .map_err(|_| {
                        format!(
                            "HLS fill range writers did not join ({})",
                            self.state.background_work_diagnostics()
                        )
                    })?;
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "background planning/cache work did not become idle ({})",
                    self.state.background_work_diagnostics()
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn case_task_ids(&self, task_tracker: &LiveTaskTracker) -> Result<Vec<String>, String> {
        let subscription = self
            .state
            .tasks
            .subscribe(&[])
            .map_err(|_| "case task registry could not be inspected".to_owned())?;
        let mut task_ids = subscription
            .snapshots()
            .iter()
            .map(|task| task.id.clone())
            .collect::<HashSet<_>>();
        if let Some(task_id) = task_tracker.task_id() {
            task_ids.insert(task_id);
        }
        let mut task_ids = task_ids.into_iter().collect::<Vec<_>>();
        task_ids.sort();
        Ok(task_ids)
    }

    fn cancel_case_tasks(&self, task_ids: &[String]) -> Result<(), String> {
        for task_id in task_ids {
            self.state
                .tasks
                .cancel_task(task_id)
                .map_err(|_| format!("case task {task_id} could not be cancelled"))?;
            self.state.cancel_hls_fill_work_for_task(task_id);
        }
        Ok(())
    }

    fn case_tasks_are_terminal(&self, task_ids: &[String]) -> Result<bool, String> {
        task_ids.iter().try_fold(true, |all_terminal, task_id| {
            let task = self
                .state
                .tasks
                .get_task(task_id)
                .map_err(|_| format!("case task {task_id} disappeared during teardown"))?;
            Ok(all_terminal
                && matches!(
                    task.state(),
                    TaskState::Succeeded
                        | TaskState::Completed
                        | TaskState::Failed
                        | TaskState::Cancelled
                ))
        })
    }

    async fn stop_listeners(&mut self) -> Result<(), String> {
        let grpc_result = abort_and_wait_listener("gRPC", self.grpc_task.take()).await;
        let media_result = abort_and_wait_listener("media", self.media_task.take()).await;
        combine_teardown_results(grpc_result, media_result)
    }
}

fn reject_symlink_components(path: &Path) -> Result<(), String> {
    let components = path.components().collect::<Vec<_>>();
    let anchor = components
        .iter()
        .position(|component| component.as_os_str() == ".codex-tmp")
        .ok_or_else(|| "ready file path is outside the ignored artifact directory".to_owned())?;
    let mut current = PathBuf::new();
    for component in &components[..=anchor] {
        current.push(component.as_os_str());
    }
    for component in &components[anchor + 1..] {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("ready file path traversed a symbolic link".to_owned());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("ready file path could not be checked".to_owned()),
        }
    }
    Ok(())
}

impl Drop for LiveTestServer {
    fn drop(&mut self) {
        if let Some(task) = &self.grpc_task {
            task.abort();
        }
        if let Some(task) = &self.media_task {
            task.abort();
        }
    }
}

async fn abort_and_wait_listener(
    name: &str,
    task: Option<JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
) -> Result<(), String> {
    let Some(task) = task else {
        return Ok(());
    };
    task.abort();
    match task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("{name} listener failed: {error}")),
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(format!("{name} listener join failed: {error}")),
    }
}

fn combine_teardown_results(
    first: Result<(), String>,
    second: Result<(), String>,
) -> Result<(), String> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(format!("{first}; {second}")),
    }
}

fn live_server_options(
    root_path: PathBuf,
    grpc_url: String,
    media_url: String,
    cache_max_bytes: u64,
) -> CacheServerOptions {
    let mut args = vec![
        "--Cache:ServerName".to_owned(),
        "Bilibili Live E2E".to_owned(),
        "--Cache:TaskStatePath".to_owned(),
        root_path
            .join(".state")
            .join("tasks.json")
            .display()
            .to_string(),
        "--Cache:RootPath".to_owned(),
        root_path.display().to_string(),
        "--Cache:GrpcListenUrl".to_owned(),
        grpc_url,
        "--Cache:MediaListenUrl".to_owned(),
        media_url,
        "--Cache:BonjourEnabled".to_owned(),
        "false".to_owned(),
        "--Cache:BilibiliWorkerEnabled".to_owned(),
        "false".to_owned(),
        "--Cache:HlsCacheMaxBytes".to_owned(),
        cache_max_bytes.to_string(),
    ];
    args.extend(live_server_environment_args(|key| env::var(key).ok()));

    CacheServerOptions::from_args(args)
        .expect("live e2e cache server options should parse")
        .normalized_for_runtime()
}

fn live_cache_budget(full_fill: bool) -> u64 {
    if full_fill {
        FULL_FILL_CACHE_BUDGET_BYTES
    } else {
        0
    }
}

fn live_server_environment_args(get_env: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut args = Vec::new();
    for (env_key, config_key) in [
        (
            "BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH",
            "Cache:BBDownCredentialPath",
        ),
        (
            "BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE",
            "Cache:BBDownCredentialProfile",
        ),
        (
            "BILIBILI_LIVE_E2E_RESTRICTED_AREA",
            "Cache:BBDownRestrictedArea",
        ),
        (
            "BILIBILI_LIVE_E2E_RESTRICTED_AREA_PROXY",
            "Cache:BBDownRestrictedAreaProxy",
        ),
        (
            "BILIBILI_LIVE_E2E_RESTRICTED_API_PROXY",
            "Cache:BBDownRestrictedApiProxy",
        ),
    ] {
        push_arg_from_value(&mut args, get_env(env_key), config_key);
    }
    args
}

fn push_arg_from_value(args: &mut Vec<String>, value: Option<String>, config_key: &str) {
    let Some(value) = value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    args.push(format!("--{config_key}"));
    args.push(value);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_duration_is_opt_in_and_requires_positive_integer_seconds() {
        assert_eq!(None, parse_sustained_duration(None).unwrap());
        assert_eq!(
            Some(Duration::from_secs(17)),
            parse_sustained_duration(Some("17")).unwrap()
        );
        assert!(parse_sustained_duration(Some("0")).is_err());
        assert!(parse_sustained_duration(Some("1.5")).is_err());
        assert!(parse_sustained_duration(Some("abc")).is_err());
    }

    #[test]
    fn full_fill_flag_is_independent_and_default_off() {
        assert!(!parse_env_flag("0"));
        assert!(!parse_env_flag("false"));
        assert!(parse_env_flag("1"));
        assert!(parse_env_flag(" YES "));
        assert!(parse_env_flag("on"));
    }

    #[test]
    fn full_fill_cache_budget_is_bounded_without_changing_default_options() {
        assert_eq!(0, live_cache_budget(false));
        assert_eq!(FULL_FILL_CACHE_BUDGET_BYTES, live_cache_budget(true));

        let root = tempfile::tempdir().expect("cache budget fixture root should exist");
        let options = live_server_options(
            root.path().to_owned(),
            "http://127.0.0.1:41000".to_owned(),
            "http://127.0.0.1:41001".to_owned(),
            live_cache_budget(true),
        );
        assert_eq!(FULL_FILL_CACHE_BUDGET_BYTES, options.hls_cache_max_bytes);

        let default_options = live_server_options(
            root.path().to_owned(),
            "http://127.0.0.1:41000".to_owned(),
            "http://127.0.0.1:41001".to_owned(),
            live_cache_budget(false),
        );
        assert_eq!(0, default_options.hls_cache_max_bytes);
    }

    #[test]
    fn offline_duration_is_independent_opt_in_and_bounded_to_180_through_300_seconds() {
        assert_eq!(None, parse_offline_duration(None).unwrap());
        assert_eq!(
            Some(Duration::from_secs(180)),
            parse_offline_duration(Some("180")).unwrap()
        );
        assert_eq!(
            Some(Duration::from_secs(300)),
            parse_offline_duration(Some("300")).unwrap()
        );
        assert!(parse_offline_duration(Some("179")).is_err());
        assert!(parse_offline_duration(Some("301")).is_err());
        assert!(parse_offline_duration(Some("300.0")).is_err());
    }

    #[test]
    fn partial_checkpoint_requires_positive_nonterminal_durable_bytes() {
        use tvos_net_player_cache_server::generated::tvos_net_player::v1::HlsCacheFillStatus;

        let mut status = HlsCacheFillStatus {
            state: HlsCacheFillState::Filling as i32,
            completed_bytes: 64,
            total_bytes: 128,
            total_bytes_known: true,
            representation_id: "repr-a".to_owned(),
            ..Default::default()
        };
        assert!(is_partial_fill_checkpoint(&status));
        status.completed_bytes = 0;
        assert!(!is_partial_fill_checkpoint(&status));
        status.completed_bytes = 128;
        assert!(!is_partial_fill_checkpoint(&status));
        status.completed_bytes = 64;
        status.state = HlsCacheFillState::Completed as i32;
        assert!(!is_partial_fill_checkpoint(&status));
    }

    #[test]
    fn offline_byte_aggregation_is_checked_and_capped() {
        assert_eq!(Some(65), bounded_byte_sum(64, 1));
        assert_eq!(None, bounded_byte_sum(u64::MAX, 1));
        assert_eq!(None, bounded_byte_sum(OFFLINE_MAX_VERIFIED_BYTES, 1));
    }

    #[test]
    fn checkpoint_snapshot_reads_verified_extents_without_exposing_manifest_data() {
        let root = tempfile::tempdir().expect("checkpoint fixture root should exist");
        let session_dir = root.path().join(".tvos-net-player/hls/session-a");
        fs::create_dir_all(&session_dir).expect("checkpoint fixture directory should exist");
        fs::write(
            session_dir.join("video.m4s.range.json"),
            serde_json::json!({
                "schema_version": 1,
                "resource_id": "video.m4s",
                "representation_digest": "repr-a",
                "extents": [{"start": 0, "end": 3, "sha256": "a".repeat(64)}]
            })
            .to_string(),
        )
        .expect("synthetic checkpoint manifest should be written");

        let snapshot = snapshot_range_checkpoints(root.path())
            .expect("synthetic checkpoint should be inspected");

        assert_eq!(4, snapshot.durable_bytes);
        assert_eq!(1, snapshot.manifests.len());
        assert_eq!(1, snapshot.manifests.values().next().unwrap().len());
    }

    #[test]
    fn quiesced_checkpoint_uses_durable_lower_bound_and_completed_state_wins() {
        use tvos_net_player_cache_server::generated::tvos_net_player::v1::HlsCacheFillStatus;

        let mut status = HlsCacheFillStatus {
            state: HlsCacheFillState::Preempted as i32,
            completed_bytes: 32,
            total_bytes: 128,
            total_bytes_known: true,
            representation_id: "repr-a".to_owned(),
            ..Default::default()
        };
        let snapshot = RangeCheckpointSnapshot {
            durable_bytes: 64,
            manifests: [(
                "session-a/video/repr-a".to_owned(),
                vec![RangeExtentCheckpoint {
                    start: 0,
                    end: 63,
                    sha256: "a".repeat(64),
                }],
            )]
            .into_iter()
            .collect(),
        };

        assert_eq!(
            Ok(QuiescedCheckpointEvidence::Partial {
                status_bytes: 32,
                durable_bytes: 64,
                extent_count: 1,
            }),
            classify_quiesced_checkpoint(Some(&status), &snapshot)
        );

        status.state = HlsCacheFillState::Completed as i32;
        assert_eq!(
            Ok(QuiescedCheckpointEvidence::Completed),
            classify_quiesced_checkpoint(Some(&status), &snapshot)
        );
        status.state = HlsCacheFillState::Preempted as i32;
        status.completed_bytes = 0;
        assert_eq!(
            Ok(QuiescedCheckpointEvidence::Inconclusive),
            classify_quiesced_checkpoint(Some(&status), &snapshot)
        );
        assert_eq!(
            Ok(QuiescedCheckpointEvidence::Inconclusive),
            classify_quiesced_checkpoint(None, &RangeCheckpointSnapshot::default())
        );
    }

    #[test]
    fn quiesced_partial_status_must_be_covered_by_durable_extents() {
        use tvos_net_player_cache_server::generated::tvos_net_player::v1::HlsCacheFillStatus;

        let status = HlsCacheFillStatus {
            state: HlsCacheFillState::Preempted as i32,
            completed_bytes: 64,
            total_bytes: 128,
            total_bytes_known: true,
            representation_id: "repr-a".to_owned(),
            ..Default::default()
        };
        let snapshot = RangeCheckpointSnapshot {
            durable_bytes: 32,
            manifests: std::collections::BTreeMap::new(),
        };

        assert_eq!(
            Err("quiesced range extents were below the typed progress lower bound"),
            classify_quiesced_checkpoint(Some(&status), &snapshot)
        );
    }

    #[test]
    fn completed_session_metadata_rejects_upstream_urls_and_headers() {
        assert!(!persisted_session_contains_upstream_material(
            &serde_json::json!({
                "variant": {"video": {"request": {"url": "", "backup_urls": [], "headers": []}}},
                "alternate_variants": []
            })
        ));
        assert!(persisted_session_contains_upstream_material(
            &serde_json::json!({
                "variant": {"video": {"request": {"url": "https://cdn.example.invalid/media"}}}
            })
        ));
        assert!(persisted_session_contains_upstream_material(
            &serde_json::json!({
                "alternate_variants": [{"audio": {"request": {"headers": [{"name": "Cookie"}]}}}]
            })
        ));
    }

    #[tokio::test]
    async fn ready_file_is_private_atomic_payload_and_removed_after_listener_shutdown() {
        use std::os::unix::fs::PermissionsExt;

        let mut server = LiveTestServer::start().await;
        let path = server
            .temp_root_path()
            .join(".codex-tmp/fill-pr4-live/ready.json");
        let payload = br#"{"phase":"cache-only-ready","uri":"http://127.0.0.1:42000/hls/item/master.m3u8","case_id":"ordinary-video-playlist","task_id":"task-a","total_bytes":1234,"expires_in_seconds":300}"#.to_vec();
        server
            .publish_ready_file(path.clone(), payload.clone())
            .expect("ready file should publish");
        assert_eq!(
            payload,
            fs::read(&path).expect("ready file should be readable")
        );
        assert_eq!(
            0o600,
            fs::metadata(&path).unwrap().permissions().mode() & 0o777
        );

        server
            .shutdown(&LiveTaskTracker::default())
            .await
            .expect("server should stop and remove ready file");
        assert!(
            !path.exists(),
            "ready file should not outlive listener shutdown"
        );
    }

    #[tokio::test]
    async fn same_root_restart_preserves_task_identity_and_task_state() {
        let mut server = LiveTestServer::start().await;
        let root = server.temp_root_path().to_owned();
        let task = server
            .state
            .tasks
            .create_bilibili_task("BV1restartIdentity", None)
            .expect("restart task should be persisted");

        server
            .restart_preserving_state()
            .await
            .expect("idle server should restart on the same root");

        assert_eq!(root, server.temp_root_path());
        assert_eq!(
            task.id,
            server
                .state
                .tasks
                .get_task(&task.id)
                .expect("persisted task should survive restart")
                .id
        );
        server
            .shutdown(&LiveTaskTracker::default())
            .await
            .expect("restarted server should shut down cleanly");
    }

    #[test]
    fn sustained_playlist_parser_tracks_explicit_and_implicit_byte_ranges() {
        let playlist = "#EXTM3U\n#EXT-X-MAP:URI=\"media.m4s\",BYTERANGE=\"16@0\"\n#EXT-X-BYTERANGE:32\n#EXTINF:1.0,\nmedia.m4s\n#EXT-X-ENDLIST\n";

        let resources = hls_probe_resources(playlist).unwrap();

        assert_eq!(
            vec![
                HlsProbeResource {
                    uri: "media.m4s".to_owned(),
                    range: Some(ByteRangeRequest { start: 0, end: 15 }),
                    initialization: true,
                },
                HlsProbeResource {
                    uri: "media.m4s".to_owned(),
                    range: Some(ByteRangeRequest { start: 16, end: 47 }),
                    initialization: false,
                },
            ],
            resources
        );
        assert_eq!(Some((16, 47)), content_range_bounds("bytes 16-47/128"));
        assert_eq!(None, content_range_bounds("bytes */128"));
    }

    #[test]
    fn content_range_parser_returns_exact_validated_bounds() {
        assert_eq!(
            Some((0, 65_535)),
            content_range_bounds("bytes 0-65535/90000")
        );
        assert_eq!(Some((128, 255)), content_range_bounds("bytes 128-255/*"));
        assert_eq!(None, content_range_bounds("bytes 256-255/90000"));
        assert_eq!(None, content_range_bounds("0-127/90000"));
    }

    #[test]
    fn sustained_probe_advances_large_ranges_then_wraps_without_reading_init_bytes() {
        let media_start = 32;
        let media_end = media_start + 2 * SUSTAINED_READ_LIMIT + 8;
        let resources = vec![
            HlsProbeResource {
                uri: "combined.m4s".to_owned(),
                range: Some(ByteRangeRequest {
                    start: 0,
                    end: media_start - 1,
                }),
                initialization: true,
            },
            HlsProbeResource {
                uri: "combined.m4s".to_owned(),
                range: Some(ByteRangeRequest {
                    start: media_start,
                    end: media_end,
                }),
                initialization: false,
            },
        ];
        let mut cursor = SustainedProbeCursor::default();

        let first = next_sustained_probe_request(&resources, &mut cursor).unwrap();
        let second = next_sustained_probe_request(&resources, &mut cursor).unwrap();
        let last = next_sustained_probe_request(&resources, &mut cursor).unwrap();
        let wrapped = next_sustained_probe_request(&resources, &mut cursor).unwrap();

        assert_eq!("combined.m4s", first.uri);
        assert_eq!(media_start, first.range.start);
        assert_eq!(media_start + SUSTAINED_READ_LIMIT - 1, first.range.end);
        assert_eq!(media_start + SUSTAINED_READ_LIMIT, second.range.start);
        assert_eq!(media_start + 2 * SUSTAINED_READ_LIMIT - 1, second.range.end);
        assert_eq!(media_start + 2 * SUSTAINED_READ_LIMIT, last.range.start);
        assert_eq!(media_end, last.range.end);
        assert_eq!(media_start, wrapped.range.start);
        assert_eq!(media_start + SUSTAINED_READ_LIMIT - 1, wrapped.range.end);
    }

    #[test]
    fn sustained_probe_steps_through_media_segments_and_wraps() {
        let resources = vec![
            HlsProbeResource {
                uri: "init.m4s".to_owned(),
                range: None,
                initialization: true,
            },
            HlsProbeResource {
                uri: "segment-1.m4s".to_owned(),
                range: None,
                initialization: false,
            },
            HlsProbeResource {
                uri: "segment-2.m4s".to_owned(),
                range: None,
                initialization: false,
            },
        ];
        let mut cursor = SustainedProbeCursor::default();

        let first = next_sustained_probe_request(&resources, &mut cursor).unwrap();
        let second = next_sustained_probe_request(&resources, &mut cursor).unwrap();
        let wrapped = next_sustained_probe_request(&resources, &mut cursor).unwrap();

        assert_eq!("segment-1.m4s", first.uri);
        assert_eq!("segment-2.m4s", second.uri);
        assert_eq!("segment-1.m4s", wrapped.uri);
        assert_eq!(0, first.range.start);
        assert_eq!(SUSTAINED_READ_LIMIT - 1, first.range.end);
    }

    #[test]
    fn sustained_probe_covers_and_rotates_audio_and_video_children() {
        let fallback = Url::parse("http://127.0.0.1:41000/master.m3u8").unwrap();
        let video = Url::parse("http://127.0.0.1:41000/video.m3u8").unwrap();
        let audio = Url::parse("http://127.0.0.1:41000/audio.m3u8").unwrap();
        let urls = unique_child_playlist_urls(
            vec![video.clone(), audio.clone(), video.clone()],
            &fallback,
        );
        let mut playlists = SustainedPlaylistSet::new(urls);

        assert_eq!(2, playlists.len());
        assert_eq!("/video.m3u8", playlists.children[0].url.path());
        assert_eq!("/audio.m3u8", playlists.children[1].url.path());
        assert!(!playlists.all_initial_children_ranged());

        for index in 0..playlists.len() {
            playlists.record_range(index);
        }

        assert!(playlists.all_initial_children_ranged());
        assert_eq!(Some(0), playlists.next_index());
        assert_eq!(Some(1), playlists.next_index());
        assert_eq!(Some(0), playlists.next_index());
        assert_eq!(Some(1), playlists.next_index());
    }

    #[test]
    fn sustained_probe_identifies_first_primary_audio_child_and_skips_init() {
        let master_url = Url::parse("http://127.0.0.1:41000/hls/task/master.m3u8").unwrap();
        let master = concat!(
            "#EXTM3U\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-0\",NAME=\"Default\",URI=\"segments/audio-primary.m3u8\"\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-1\",NAME=\"Variant 1\",URI=\"segments/audio-alternate.m3u8\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"audio-0\"\n",
            "segments/video-primary.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=2000,AUDIO=\"audio-1\"\n",
            "segments/video-alternate.m3u8\n",
        );
        let roles = sustained_child_playlist_roles(master, &master_url);
        let children = playlist_referenced_uris(master)
            .into_iter()
            .filter_map(|uri| master_url.join(&uri).ok())
            .collect::<Vec<_>>();
        let first = children.first().unwrap();
        assert_eq!(Some(&"audio_primary".to_owned()), roles.get(first.as_str()));

        let resources = hls_probe_resources(
            "#EXTM3U\n#EXT-X-MAP:URI=\"segments/audio.m4s\",BYTERANGE=\"16@0\"\n#EXT-X-BYTERANGE:64@16\n#EXTINF:1.0,\nsegments/audio.m4s\n",
        )
        .unwrap();
        let request =
            next_sustained_probe_request(&resources, &mut SustainedProbeCursor::default()).unwrap();
        assert_eq!("segments/audio.m4s", request.uri);
        assert_eq!(16, request.range.start);
        assert_eq!(79, request.range.end);
    }

    #[test]
    fn sustained_playlist_parser_rejects_invalid_byte_ranges() {
        assert!(hls_probe_resources("#EXTM3U\n#EXT-X-BYTERANGE:0@0\nseg.m4s\n").is_err());
        assert!(
            hls_probe_resources("#EXTM3U\n#EXT-X-MAP:URI=\"seg.m4s\",BYTERANGE=\"bad\"\n").is_err()
        );
    }

    #[test]
    fn lan_media_origin_diagnostic_omits_credential_query() {
        // Synthetic token fixture: joey-private-v3/access-a.
        let synthetic_access_token = "codex_synth_v1_access_a";
        let url = Url::parse(&format!(
            "https://upstream.example.test/media/video.m4s?access_key={synthetic_access_token}"
        ))
        .expect("synthetic upstream URL should parse");

        let diagnostic = lan_media_origin_for_diagnostic(&url);

        assert_eq!("https://upstream.example.test", diagnostic);
        assert!(!diagnostic.contains(synthetic_access_token));
        assert!(!diagnostic.contains("access_key"));
    }

    #[tokio::test]
    async fn shutdown_cancels_untracked_registry_task_before_removing_case_root() {
        let server = LiveTestServer::start().await;
        let root_path = server.temp_root_path().to_owned();
        let tasks = Arc::clone(&server.state.tasks);
        let task = tasks
            .create_bilibili_task("BV1xx411c7mD", None)
            .expect("untracked case task should be created");
        let task_tracker = LiveTaskTracker::default();

        assert_eq!(TaskState::Queued, task.state());
        assert!(task_tracker.task_id().is_none());

        server
            .shutdown(&task_tracker)
            .await
            .expect("case shutdown should discover and cancel untracked tasks");

        assert_eq!(
            TaskState::Cancelled,
            tasks
                .get_task(&task.id)
                .expect("cancelled task should remain in the registry")
                .state()
        );
        assert!(
            !root_path.exists(),
            "case root should be removed only after listeners and background work stop"
        );
    }

    #[tokio::test]
    async fn teardown_failure_retains_case_root_and_persisted_state_for_recovery() {
        let mut server = LiveTestServer::start().await;
        let root_path = server.temp_root_path().to_owned();
        let task_state_path = root_path.join(".state").join("tasks.json");
        server
            .state
            .tasks
            .create_bilibili_task("BV1retained-state", None)
            .expect("diagnostic task state should be persisted");
        assert!(task_state_path.exists());
        server
            .stop_listeners()
            .await
            .expect("test listeners should stop cleanly");

        let error = server
            .finish_teardown(Err("forced teardown failure".to_owned()))
            .expect_err("forced teardown failure should be reported");

        assert!(error.contains("forced teardown failure"));
        assert!(error.contains(&root_path.display().to_string()));
        assert!(
            root_path.exists(),
            "failed teardown should retain the isolated root and persisted state"
        );
        assert!(task_state_path.exists());
        fs::remove_dir_all(&root_path).expect("retained test root should be removable");
    }

    #[test]
    fn live_server_environment_args_maps_named_credential_profile() {
        let args = live_server_environment_args(|key| match key {
            "BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE" => Some("family-room".to_owned()),
            _ => None,
        });

        assert_eq!(
            vec![
                "--Cache:BBDownCredentialProfile".to_owned(),
                "family-room".to_owned(),
            ],
            args
        );
    }

    #[test]
    fn fixture_set_includes_authenticated_page_fetch_cases() {
        let fixture_set = LiveFixtureSet::load_from_path(default_fixture_path());
        let cases = fixture_set
            .cases
            .iter()
            .map(|case| (case.id.as_str(), case))
            .collect::<std::collections::HashMap<_, _>>();

        for (id, source_kind) in [
            ("authenticated-history", "history"),
            ("authenticated-watch-later", "watch_later"),
            ("authenticated-following-feed", "following"),
            ("authenticated-space-dynamic", "space_dynamic"),
        ] {
            let case = cases.get(id).unwrap_or_else(|| panic!("missing {id}"));
            assert!(case.requires_authentication, "{id} should require auth");
            assert_eq!(case.expected_source_kind, source_kind);
            assert!(case.minimum_candidates >= 1);
        }

        assert_eq!(
            cases["authenticated-space-dynamic"].url_env.as_deref(),
            Some("BILIBILI_LIVE_E2E_SPACE_DYNAMIC_URL")
        );
    }

    #[test]
    fn fixture_set_includes_collection_list_fetch_cases() {
        let fixture_set = LiveFixtureSet::load_from_path(default_fixture_path());
        let cases = fixture_set
            .cases
            .iter()
            .map(|case| (case.id.as_str(), case))
            .collect::<std::collections::HashMap<_, _>>();

        for (id, source_kind, env_key, selection) in [
            (
                "favorite-list",
                "favorite",
                "BILIBILI_LIVE_E2E_FAVORITE_URL",
                SelectionPolicy::First,
            ),
            (
                "space-videos",
                "space",
                "BILIBILI_LIVE_E2E_SPACE_VIDEOS_URL",
                SelectionPolicy::RangeFirstTwo,
            ),
            (
                "space-collection",
                "collection",
                "BILIBILI_LIVE_E2E_COLLECTION_URL",
                SelectionPolicy::MultipleFirstTwo,
            ),
            (
                "space-series",
                "series",
                "BILIBILI_LIVE_E2E_SERIES_URL",
                SelectionPolicy::First,
            ),
            (
                "homepage-recommendations",
                "recommendation",
                "BILIBILI_LIVE_E2E_RECOMMENDATIONS_URL",
                SelectionPolicy::MultipleFirstTwo,
            ),
        ] {
            let case = cases.get(id).unwrap_or_else(|| panic!("missing {id}"));
            assert_eq!(case.expected_source_kind, source_kind);
            assert_eq!(
                case.expected_candidate_source_kind.as_deref(),
                Some(source_kind)
            );
            assert_eq!(case.url_env.as_deref(), Some(env_key));
            assert_eq!(case.selection, selection);
            assert!(case.requires_collection_list_validation);
            assert!(case.requires_stable_item_selection);
            assert!(case.minimum_candidates >= 1);
        }

        assert!(cases["space-videos"].requires_authentication);
        assert!(cases["homepage-recommendations"].requires_authentication);
        assert!(cases["favorite-list"].requires_live_sample_override);
        assert!(cases["space-series"].requires_live_sample_override);
        assert!(!cases["space-collection"].requires_live_sample_override);
        assert_eq!(cases["space-series"].timeout_seconds, Some(180));
    }

    #[test]
    fn stable_item_candidate_contract_accepts_complete_identity() {
        let case = test_case("space-videos", false, false);
        let valid = BilibiliResolutionCandidate {
            candidate_token: "opaque-server-token".to_owned(),
            source_kind: "space".to_owned(),
            identity: Some(BilibiliContentIdentity {
                kind: BilibiliContentKind::CollectionItem.into(),
                aid: 170001,
                bvid: "BV1xx411c7mD".to_owned(),
                cid: 270001,
                ..Default::default()
            }),
            index: 1,
            ..Default::default()
        };

        assert_stable_item_candidate(&case, &valid);
    }

    #[test]
    fn generic_artifact_reference_must_be_server_owned_and_credential_free() {
        let case = test_case("ordinary-video-playlist", false, false);
        let media_url = "http://127.0.0.1:41000";
        let accepted = TaskArtifact {
            resource: Some(CacheResourceRef {
                id: "resource-opaque-id".to_owned(),
                uri: format!("{media_url}/resources/resource-opaque-id"),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_artifact_reference_safe(&case, &accepted, media_url);

        for uri in [
            "https://upstream.example.test/video.m4s",
            "http://127.0.0.1:41000/resources/id?token=value",
        ] {
            let artifact = TaskArtifact {
                resource: Some(CacheResourceRef {
                    id: "resource-opaque-id".to_owned(),
                    uri: uri.to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert!(
                std::panic::catch_unwind(|| {
                    assert_artifact_reference_safe(&case, &artifact, media_url)
                })
                .is_err(),
                "unsafe artifact reference should be rejected"
            );
        }
    }

    #[test]
    fn run_policy_skips_authenticated_cases_by_default() {
        let policy = LiveRunPolicy::default();
        let case = test_case("authenticated-history", true, false);

        assert_eq!(
            policy.run_decision(&case),
            LiveRunDecision::Skip("requires authenticated live validation")
        );
    }

    #[test]
    fn run_policy_runs_authenticated_cases_when_included() {
        let policy = LiveRunPolicy {
            filter: None,
            include_authenticated: true,
            include_collection_list: false,
            sustained_duration: None,
            ..Default::default()
        };
        let case = test_case("authenticated-history", true, false);

        assert_eq!(policy.run_decision(&case), LiveRunDecision::Run);
    }

    #[test]
    fn run_policy_explicit_filter_runs_authenticated_cases() {
        let policy = LiveRunPolicy {
            filter: Some(parse_case_filter("authenticated-history".to_owned())),
            include_authenticated: false,
            include_collection_list: false,
            sustained_duration: None,
            ..Default::default()
        };
        let case = test_case("authenticated-history", true, false);

        assert_eq!(policy.run_decision(&case), LiveRunDecision::Run);
    }

    #[test]
    fn run_policy_skips_collection_list_cases_by_default() {
        let policy = LiveRunPolicy::default();
        let mut case = test_case("space-collection", false, false);
        case.requires_collection_list_validation = true;

        assert_eq!(
            policy.run_decision(&case),
            LiveRunDecision::Skip("requires explicit collection/list live validation")
        );
    }

    #[test]
    fn run_policy_runs_collection_list_cases_when_included() {
        let policy = LiveRunPolicy {
            filter: None,
            include_authenticated: false,
            include_collection_list: true,
            sustained_duration: None,
            ..Default::default()
        };
        let mut case = test_case("space-collection", false, false);
        case.requires_collection_list_validation = true;

        assert_eq!(policy.run_decision(&case), LiveRunDecision::Run);
    }

    #[test]
    fn run_policy_collection_list_include_still_skips_authenticated_collection_cases() {
        let policy = LiveRunPolicy {
            filter: None,
            include_authenticated: false,
            include_collection_list: true,
            sustained_duration: None,
            ..Default::default()
        };
        let mut case = test_case("space-videos", true, false);
        case.requires_collection_list_validation = true;

        assert_eq!(
            policy.run_decision(&case),
            LiveRunDecision::Skip("requires authenticated live validation")
        );
    }

    #[test]
    fn run_policy_collection_list_and_authenticated_include_runs_authenticated_collection_cases() {
        let policy = LiveRunPolicy {
            filter: None,
            include_authenticated: true,
            include_collection_list: true,
            sustained_duration: None,
            ..Default::default()
        };
        let mut case = test_case("space-videos", true, false);
        case.requires_collection_list_validation = true;

        assert_eq!(policy.run_decision(&case), LiveRunDecision::Run);
    }

    #[test]
    fn run_policy_collection_list_include_skips_cases_that_need_sample_override() {
        let policy = LiveRunPolicy {
            filter: None,
            include_authenticated: false,
            include_collection_list: true,
            sustained_duration: None,
            ..Default::default()
        };
        let mut case = test_case("favorite-list", false, false);
        case.url_env = Some("BILIBILI_LIVE_E2E_TEST_SOURCE_OVERRIDE_DO_NOT_SET".to_owned());
        case.requires_collection_list_validation = true;
        case.requires_live_sample_override = true;

        assert_eq!(
            policy.run_decision(&case),
            LiveRunDecision::Skip("requires live sample URL override")
        );
    }

    #[test]
    fn run_policy_explicit_filter_runs_cases_that_need_sample_override() {
        let policy = LiveRunPolicy {
            filter: Some(parse_case_filter("favorite-list".to_owned())),
            include_authenticated: false,
            include_collection_list: false,
            sustained_duration: None,
            ..Default::default()
        };
        let mut case = test_case("favorite-list", false, false);
        case.requires_collection_list_validation = true;
        case.requires_live_sample_override = true;

        assert_eq!(policy.run_decision(&case), LiveRunDecision::Run);
    }

    #[test]
    fn run_policy_explicit_filter_runs_collection_list_cases() {
        let policy = LiveRunPolicy {
            filter: Some(parse_case_filter("space-collection".to_owned())),
            include_authenticated: false,
            include_collection_list: false,
            sustained_duration: None,
            ..Default::default()
        };
        let mut case = test_case("space-collection", false, false);
        case.requires_collection_list_validation = true;

        assert_eq!(policy.run_decision(&case), LiveRunDecision::Run);
    }

    #[test]
    fn run_policy_still_skips_restricted_cases_by_default() {
        let policy = LiveRunPolicy::default();
        let case = test_case("bangumi-media-series", false, true);

        assert!(!policy.full_fill, "live full-fill must remain opt-in");
        assert_eq!(
            policy.run_decision(&case),
            LiveRunDecision::Skip("requires explicit restricted-area live validation")
        );
    }

    #[test]
    fn full_fill_policy_runs_explicitly_selected_bangumi_single_result_cases() {
        let mut media_policy = LiveRunPolicy {
            full_fill: true,
            filter: Some(parse_case_filter("bangumi-media-series".to_owned())),
            ..Default::default()
        };
        let media_case = test_case("bangumi-media-series", false, true);
        assert_eq!(media_policy.run_decision(&media_case), LiveRunDecision::Run);

        media_policy.filter = Some(parse_case_filter("bangumi-episode".to_owned()));
        let mut episode_case = test_case("bangumi-episode", false, true);
        episode_case.selection = SelectionPolicy::DefaultOrFirst;
        assert_eq!(
            media_policy.run_decision(&episode_case),
            LiveRunDecision::Run
        );
    }

    #[test]
    fn full_fill_policy_skips_multi_result_and_unknown_cases() {
        let mut policy = LiveRunPolicy {
            full_fill: true,
            filter: Some(parse_case_filter("multi-part-video".to_owned())),
            ..Default::default()
        };
        let mut multi_result_case = test_case("multi-part-video", false, false);
        multi_result_case.selection = SelectionPolicy::RangeFirstTwo;
        assert_eq!(
            policy.run_decision(&multi_result_case),
            LiveRunDecision::Skip(
                "full-fill mode requires an allowlisted canonical single-result case"
            )
        );

        policy.filter = Some(parse_case_filter("unrecognized-case".to_owned()));
        let unknown_case = test_case("unrecognized-case", false, false);
        assert_eq!(
            policy.run_decision(&unknown_case),
            LiveRunDecision::Skip(
                "full-fill mode requires an allowlisted canonical single-result case"
            )
        );
    }

    #[test]
    fn failure_classification_prefers_missing_credentials_for_auth_cases() {
        let case = test_case("authenticated-history", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: false,
            ..Default::default()
        };

        assert_eq!(
            classify_live_failure(&case, "resolve", "upstream error", Some(&status)),
            LiveFailureClass::Credential
        );
    }

    #[test]
    fn failure_classification_labels_empty_account_state() {
        let case = test_case("authenticated-watch-later", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        assert_eq!(
            classify_live_failure(
                &case,
                "resolve",
                "expected candidates, got 0",
                Some(&status)
            ),
            LiveFailureClass::EmptyAccountState
        );
    }

    #[test]
    fn live_failure_message_labels_empty_resolved_candidates() {
        let case = test_case("authenticated-watch-later", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        let message = live_failure_message(
            &case,
            "resolve candidates",
            "expected at least 1 candidates, got 0",
            Some(&status),
        );

        assert!(message.contains("[empty_account_state]"));
    }

    #[test]
    fn failure_classification_keeps_public_zero_candidates_as_upstream() {
        let case = test_case("ordinary-video-playlist", false, false);

        assert_eq!(
            classify_live_failure(
                &case,
                "resolve candidates",
                "expected candidates, got 0",
                None
            ),
            LiveFailureClass::UpstreamSchemaOrAvailability
        );
    }

    #[test]
    fn failure_classification_labels_stale_dynamic_selection_as_upstream() {
        let case = test_case("authenticated-following-feed", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        assert_eq!(
            classify_live_failure(
                &case,
                "create",
                "Selected Bilibili item BV1xx was not found in resolved candidates",
                Some(&status)
            ),
            LiveFailureClass::UpstreamSchemaOrAvailability
        );
        assert_eq!(
            classify_live_failure(
                &case,
                "create",
                "selected item no longer matches the resolved candidate",
                Some(&status)
            ),
            LiveFailureClass::UpstreamSchemaOrAvailability
        );
    }

    #[test]
    fn failure_classification_labels_restricted_proxy_errors() {
        let case = test_case("bangumi-episode", false, true);

        assert_eq!(
            classify_live_failure(&case, "resolve", "region restricted", None),
            LiveFailureClass::RestrictedProxy
        );
    }

    #[test]
    fn failure_classification_labels_generic_resolve_as_upstream() {
        let case = test_case("authenticated-following-feed", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        assert_eq!(
            classify_live_failure(&case, "resolve", "unexpected JSON shape", Some(&status)),
            LiveFailureClass::UpstreamSchemaOrAvailability
        );
    }

    #[test]
    fn failure_classification_labels_background_planning_upstream_errors() {
        let case = test_case("authenticated-following-feed", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        for detail in [
            "BBDown resolve failed: upstream schema changed",
            "playurl request failed with HTTP status 503",
            "network connection timed out while fetching Bilibili page",
        ] {
            assert_eq!(
                classify_live_failure(&case, "task ended in Failed", detail, Some(&status)),
                LiveFailureClass::UpstreamSchemaOrAvailability,
                "{detail}"
            );
        }
    }

    #[test]
    fn task_failure_message_classifies_child_result_without_exposing_raw_detail() {
        let case = test_case("bangumi-media-series", false, true);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            access_key_present: true,
            ..Default::default()
        };
        let task = Task {
            message: "request failed with parent-sensitive-marker".to_owned(),
            result_items: vec![BilibiliTaskResultItem {
                state: TaskState::Failed.into(),
                message: "restricted proxy rejected child-sensitive-marker".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let message =
            live_task_failure_message(&case, "task ended in Failed", &task, Some(&status));

        assert!(message.contains("[restricted_proxy]"));
        assert!(message.contains("failed_result_count=1"));
        assert!(!message.contains("parent-sensitive-marker"));
        assert!(!message.contains("child-sensitive-marker"));
    }

    #[test]
    fn task_failure_message_uses_typed_rpc_class_after_detail_redaction() {
        let case = test_case("bangumi-media-series", false, true);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            access_key_present: true,
            ..Default::default()
        };
        let task = Task {
            message: format!("{CREDENTIAL_SAFE_CLIENT_DETAIL} [bilibili_failure_class=server_bug]"),
            result_items: vec![BilibiliTaskResultItem {
                state: TaskState::Failed.into(),
                message: format!(
                    "{CREDENTIAL_SAFE_CLIENT_DETAIL} [bilibili_failure_class=restricted_proxy]"
                ),
                ..Default::default()
            }],
            ..Default::default()
        };

        let message =
            live_task_failure_message(&case, "task ended in Failed", &task, Some(&status));

        assert!(message.contains("[restricted_proxy]"));
        assert!(message.contains("failed_result_count=1"));
    }

    #[test]
    fn authenticated_failure_prefers_typed_upstream_class_over_safe_marker_wording() {
        let case = test_case("authenticated-history", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        assert_eq!(
            LiveFailureClass::UpstreamSchemaOrAvailability,
            classify_live_failure(
                &case,
                "task ended in Failed",
                &format!(
                    "{CREDENTIAL_SAFE_CLIENT_DETAIL} [bilibili_failure_class=upstream_schema_or_availability]"
                ),
                Some(&status),
            )
        );
    }

    #[test]
    fn failure_classification_does_not_trust_raw_upstream_class_tag() {
        let case = test_case("authenticated-history", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        assert_eq!(
            LiveFailureClass::UpstreamSchemaOrAvailability,
            classify_live_failure(
                &case,
                "task ended in Failed",
                "upstream request failed [bilibili_failure_class=credential]",
                Some(&status),
            )
        );
    }

    #[test]
    fn live_failure_message_omits_raw_detail_when_credentials_are_loaded() {
        let case = test_case("authenticated-history", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        let message = live_failure_message(
            &case,
            "resolve",
            "login cookie rejected credential-sensitive-marker",
            Some(&status),
        );

        assert!(message.contains("[credential]"));
        assert!(message.contains("upstream detail omitted"));
        assert!(!message.contains("credential-sensitive-marker"));
        assert!(!message.contains("cookie"));
    }

    #[test]
    fn live_failure_message_omits_raw_detail_when_credential_path_fails_to_load() {
        let case = test_case("bangumi-episode", false, true);
        let status = BilibiliCredentialStatus {
            credential_path_configured: true,
            credential_file_loaded: false,
            ..Default::default()
        };

        let message = live_failure_message(
            &case,
            "resolve",
            "failed to read /private/credential-sensitive-marker.json",
            Some(&status),
        );

        assert!(message.contains("upstream detail omitted"));
        assert!(!message.contains("credential-sensitive-marker"));
        assert!(!message.contains("/private/"));
    }

    #[test]
    fn failure_classification_keeps_stalled_preparing_state_as_server_bug() {
        let case = test_case("authenticated-following-feed", true, false);
        let status = BilibiliCredentialStatus {
            credential_file_loaded: true,
            web_cookie_present: true,
            ..Default::default()
        };

        assert_eq!(
            classify_live_failure(
                &case,
                "task did not become playable",
                "Preparing Bilibili playback plan.",
                Some(&status)
            ),
            LiveFailureClass::ServerBug
        );
    }

    #[test]
    fn failure_classification_labels_non_resolve_generic_as_server_bug() {
        let case = test_case("ordinary-video-playlist", false, false);

        assert_eq!(
            classify_live_failure(&case, "task ended in Failed", "playlist write failed", None),
            LiveFailureClass::ServerBug
        );
    }

    fn test_case(
        id: impl Into<String>,
        requires_authentication: bool,
        requires_restricted_area_path: bool,
    ) -> LiveCase {
        LiveCase {
            id: id.into(),
            url: "https://www.bilibili.com/account/history".to_owned(),
            url_env: None,
            expected_source_kind: "history".to_owned(),
            expected_candidate_source_kind: None,
            minimum_candidates: 1,
            selection: SelectionPolicy::First,
            requires_restricted_area_path,
            requires_authentication,
            requires_collection_list_validation: false,
            requires_stable_item_selection: false,
            requires_live_sample_override: false,
            playback_options: LivePlaybackOptions {
                quality_preference: "360p".to_owned(),
                encoding_preference: "h264".to_owned(),
                prefer_tv_api: false,
                audio_language: String::new(),
            },
            timeout_seconds: None,
        }
    }
}
