use std::{collections::HashSet, env, fs, path::PathBuf, time::Duration};

use reqwest::StatusCode;
use serde::Deserialize;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tonic::Request;
use tvos_net_player_cache_server::{
    AppState,
    config::CacheServerOptions,
    generated::tvos_net_player::v1::{
        BilibiliPlaybackOptions, BilibiliTaskSelection, CancelTaskRequest,
        CreateBilibiliPlaybackTaskRequest, GetTaskRequest, PlaybackProtocol, PlaybackSource,
        ResolveBilibiliInputRequest, Task, TaskState, task_service_client::TaskServiceClient,
    },
    run_grpc_listener, run_media_listener,
};

const BILIBILI_TASK_SELECTION_MODE_SINGLE: i32 = 3;
const BILIBILI_TASK_SELECTION_MODE_MULTIPLE: i32 = 4;
const BILIBILI_TASK_SELECTION_MODE_RANGE: i32 = 5;
const BILIBILI_TASK_SELECTION_MODE_ALL: i32 = 6;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires live Bilibili network access and is intentionally outside default CI"]
async fn bilibili_live_cases_resolve_and_create_playable_hls() {
    let fixture_set = LiveFixtureSet::load();
    let filter = case_filter_from_env();
    let server = LiveTestServer::start().await;
    let http = reqwest::Client::new();
    let mut ran_cases = 0usize;

    for case in fixture_set.cases.iter().filter(|case| {
        filter
            .as_ref()
            .is_none_or(|filter| filter.contains(&case.id))
    }) {
        if filter.is_none() && case.requires_restricted_area_path {
            println!(
                "skipping {}: requires explicit restricted-area live validation",
                case.id
            );
            continue;
        }

        ran_cases += 1;
        println!("running {}", case.id);
        run_live_case(case, server.channel().await, &http).await;
    }

    assert!(
        ran_cases > 0,
        "no live Bilibili e2e cases matched the filter"
    );
}

async fn run_live_case(
    case: &LiveCase,
    channel: tonic::transport::Channel,
    http: &reqwest::Client,
) {
    let mut task_client = TaskServiceClient::new(channel);
    let options = case.playback_options.to_proto();

    let resolved = task_client
        .resolve_bilibili_input(Request::new(ResolveBilibiliInputRequest {
            url_or_id: case.url.clone(),
            options: Some(options.clone()),
        }))
        .await
        .unwrap_or_else(|error| panic!("{}: resolve failed: {error}", case.id))
        .into_inner();

    assert!(
        !resolved.title.trim().is_empty(),
        "{}: resolved title is empty",
        case.id
    );
    assert_eq!(
        case.expected_source_kind, resolved.source_kind,
        "{}: unexpected source kind",
        case.id
    );
    assert!(
        resolved.candidates.len() >= case.minimum_candidates,
        "{}: expected at least {} candidates, got {}",
        case.id,
        case.minimum_candidates,
        resolved.candidates.len()
    );

    let selection = case.selection_request(&resolved);
    let created = task_client
        .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
            url_or_id: case.url.clone(),
            options: Some(options),
            selection_id: selection.legacy_selection_id,
            selection: Some(selection.selection),
        }))
        .await
        .unwrap_or_else(|error| panic!("{}: create playback task failed: {error}", case.id))
        .into_inner();

    let playable = wait_for_playable_task(
        &mut task_client,
        case,
        &created.id,
        selection.expected_playable_results,
    )
    .await;
    let source = playable
        .playback_source
        .as_ref()
        .unwrap_or_else(|| panic!("{}: playable task has no playback source", case.id));
    assert_hls_master(case, http, source, "task playback source").await;

    let result_sources = playable_result_sources(&playable);
    if selection.expected_playable_results > 1 {
        assert!(
            result_sources.len() >= selection.expected_playable_results,
            "{}: expected at least {} playable result items, got {}",
            case.id,
            selection.expected_playable_results,
            result_sources.len()
        );
    }
    for (index, result_source) in result_sources.into_iter().enumerate() {
        assert_hls_master(
            case,
            http,
            result_source,
            &format!("result item {} playback source", index + 1),
        )
        .await;
    }

    let _ = task_client
        .cancel_task(Request::new(CancelTaskRequest { id: playable.id }))
        .await;
}

async fn wait_for_playable_task(
    task_client: &mut TaskServiceClient<tonic::transport::Channel>,
    case: &LiveCase,
    task_id: &str,
    expected_playable_results: usize,
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
                if task_has_expected_playable_sources(&task, expected_playable_results) =>
            {
                return task;
            }
            TaskState::Failed | TaskState::Cancelled => {
                panic!(
                    "{}: task ended in {:?}: {}",
                    case.id,
                    task.state(),
                    task.message
                );
            }
            _ if tokio::time::Instant::now() >= deadline => {
                panic!(
                    "{}: task did not become playable within {:?}; last state {:?}: {}",
                    case.id,
                    timeout,
                    task.state(),
                    task.message
                );
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

fn task_has_expected_playable_sources(task: &Task, expected_playable_results: usize) -> bool {
    if expected_playable_results <= 1 {
        return task.playback_source.is_some() || !playable_result_sources(task).is_empty();
    }

    playable_result_sources(task).len() >= expected_playable_results
}

fn playable_result_sources(task: &Task) -> Vec<&PlaybackSource> {
    task.result_items
        .iter()
        .filter(|item| {
            item.state == i32::from(TaskState::Playable)
                || item.state == i32::from(TaskState::Completed)
        })
        .filter_map(|item| item.playback_source.as_ref())
        .collect()
}

async fn assert_hls_master(
    case: &LiveCase,
    http: &reqwest::Client,
    source: &PlaybackSource,
    label: &str,
) {
    assert_eq!(
        PlaybackProtocol::Hls,
        source.protocol(),
        "{}: {label} is not HLS",
        case.id
    );

    let response =
        http.get(&source.uri).send().await.unwrap_or_else(|error| {
            panic!("{}: {label} HLS master request failed: {error}", case.id)
        });
    assert_eq!(
        StatusCode::OK,
        response.status(),
        "{}: {label} HLS master returned unexpected status",
        case.id
    );
    let playlist = response
        .text()
        .await
        .unwrap_or_else(|error| panic!("{}: {label} HLS master body failed: {error}", case.id));
    assert!(
        playlist.contains("#EXTM3U"),
        "{}: {label} HLS master is not an m3u8 playlist",
        case.id
    );
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
    expected_source_kind: String,
    minimum_candidates: usize,
    selection: SelectionPolicy,
    #[serde(default)]
    requires_restricted_area_path: bool,
    playback_options: LivePlaybackOptions,
    timeout_seconds: Option<u64>,
}

impl LiveCase {
    fn selection_request(
        &self,
        resolved: &tvos_net_player_cache_server::generated::tvos_net_player::v1::BilibiliResolveResult,
    ) -> LiveSelectionRequest {
        match self.selection {
            SelectionPolicy::DefaultOrFirst => {
                let selection_id = resolved
                    .default_selection_id
                    .trim()
                    .to_owned()
                    .if_empty_then(|| first_candidate_id(self, resolved));
                LiveSelectionRequest::single(selection_id)
            }
            SelectionPolicy::First => {
                LiveSelectionRequest::single(first_candidate_id(self, resolved))
            }
            SelectionPolicy::MultipleFirstTwo => {
                LiveSelectionRequest::multiple(first_candidate_ids(self, resolved, 2))
            }
            SelectionPolicy::RangeFirstTwo => {
                let candidates = first_candidates(self, resolved, 2);
                LiveSelectionRequest::range(
                    candidates[0].index.max(1),
                    candidates[1].index.max(1),
                    2,
                )
            }
            SelectionPolicy::All => LiveSelectionRequest::all(resolved.candidates.len()),
        }
    }
}

struct LiveSelectionRequest {
    legacy_selection_id: String,
    selection: BilibiliTaskSelection,
    expected_playable_results: usize,
}

impl LiveSelectionRequest {
    fn single(selection_id: String) -> Self {
        Self {
            legacy_selection_id: String::new(),
            selection: BilibiliTaskSelection {
                mode: BILIBILI_TASK_SELECTION_MODE_SINGLE,
                selection_ids: vec![selection_id],
                range_start_index: 0,
                range_end_index: 0,
            },
            expected_playable_results: 1,
        }
    }

    fn multiple(selection_ids: Vec<String>) -> Self {
        let expected_playable_results = selection_ids.len();
        Self {
            legacy_selection_id: String::new(),
            selection: BilibiliTaskSelection {
                mode: BILIBILI_TASK_SELECTION_MODE_MULTIPLE,
                selection_ids,
                range_start_index: 0,
                range_end_index: 0,
            },
            expected_playable_results,
        }
    }

    fn range(start_index: u32, end_index: u32, expected_playable_results: usize) -> Self {
        Self {
            legacy_selection_id: String::new(),
            selection: BilibiliTaskSelection {
                mode: BILIBILI_TASK_SELECTION_MODE_RANGE,
                selection_ids: Vec::new(),
                range_start_index: start_index,
                range_end_index: end_index,
            },
            expected_playable_results,
        }
    }

    fn all(expected_playable_results: usize) -> Self {
        Self {
            legacy_selection_id: String::new(),
            selection: BilibiliTaskSelection {
                mode: BILIBILI_TASK_SELECTION_MODE_ALL,
                selection_ids: Vec::new(),
                range_start_index: 0,
                range_end_index: 0,
            },
            expected_playable_results,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
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
}

impl LivePlaybackOptions {
    fn to_proto(&self) -> BilibiliPlaybackOptions {
        BilibiliPlaybackOptions {
            quality_preference: self.quality_preference.clone(),
            encoding_preference: self.encoding_preference.clone(),
            prefer_tv_api: self.prefer_tv_api,
        }
    }
}

trait EmptyStringExt {
    fn if_empty_then(self, fallback: impl FnOnce() -> String) -> String;
}

impl EmptyStringExt for String {
    fn if_empty_then(self, fallback: impl FnOnce() -> String) -> String {
        if self.is_empty() { fallback() } else { self }
    }
}

fn first_candidate_id(
    case: &LiveCase,
    resolved: &tvos_net_player_cache_server::generated::tvos_net_player::v1::BilibiliResolveResult,
) -> String {
    resolved
        .candidates
        .first()
        .unwrap_or_else(|| panic!("{}: resolved no selectable candidates", case.id))
        .selection_id
        .clone()
}

fn first_candidate_ids(
    case: &LiveCase,
    resolved: &tvos_net_player_cache_server::generated::tvos_net_player::v1::BilibiliResolveResult,
    count: usize,
) -> Vec<String> {
    first_candidates(case, resolved, count)
        .into_iter()
        .map(|candidate| candidate.selection_id.clone())
        .collect()
}

fn first_candidates<'a>(
    case: &LiveCase,
    resolved: &'a tvos_net_player_cache_server::generated::tvos_net_player::v1::BilibiliResolveResult,
    count: usize,
) -> Vec<&'a tvos_net_player_cache_server::generated::tvos_net_player::v1::BilibiliResolvedCandidate>
{
    assert!(
        resolved.candidates.len() >= count,
        "{}: expected at least {} candidates, got {}",
        case.id,
        count,
        resolved.candidates.len()
    );
    resolved.candidates.iter().take(count).collect()
}

fn default_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".agents/skills/bilibili-live-e2e/references/live-cases.json")
}

fn case_filter_from_env() -> Option<HashSet<String>> {
    env::var("BILIBILI_LIVE_E2E_CASES")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<HashSet<_>>()
        })
        .filter(|values| !values.is_empty())
}

struct LiveTestServer {
    _temp_root: TempDir,
    grpc_url: String,
    _grpc_task: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    _media_task: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
}

impl LiveTestServer {
    async fn start() -> Self {
        let temp_root = tempfile::tempdir().unwrap();
        let root_path = temp_root.path().canonicalize().unwrap();
        let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_url = format!("http://{}", grpc_listener.local_addr().unwrap());
        let media_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let media_url = format!("http://{}", media_listener.local_addr().unwrap());
        let options = live_server_options(root_path.clone(), grpc_url.clone(), media_url);
        let state = AppState::new(options);

        let grpc_task = tokio::spawn(run_grpc_listener(grpc_listener, state.clone()));
        let media_task = tokio::spawn(run_media_listener(media_listener, state));

        wait_for_grpc(&grpc_url).await;
        Self {
            _temp_root: temp_root,
            grpc_url,
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

fn live_server_options(
    root_path: PathBuf,
    grpc_url: String,
    media_url: String,
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
        "0".to_owned(),
    ];
    push_arg_from_env(
        &mut args,
        "BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH",
        "Cache:BBDownCredentialPath",
    );
    push_arg_from_env(
        &mut args,
        "BILIBILI_LIVE_E2E_RESTRICTED_AREA",
        "Cache:BBDownRestrictedArea",
    );
    push_arg_from_env(
        &mut args,
        "BILIBILI_LIVE_E2E_RESTRICTED_AREA_PROXY",
        "Cache:BBDownRestrictedAreaProxy",
    );
    push_arg_from_env(
        &mut args,
        "BILIBILI_LIVE_E2E_RESTRICTED_API_PROXY",
        "Cache:BBDownRestrictedApiProxy",
    );

    CacheServerOptions::from_args(args)
        .expect("live e2e cache server options should parse")
        .normalized_for_runtime()
}

fn push_arg_from_env(args: &mut Vec<String>, env_key: &str, config_key: &str) {
    let Some(value) = env::var(env_key)
        .ok()
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
