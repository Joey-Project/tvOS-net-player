use std::{
    collections::HashSet,
    env, fs,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::FutureExt;
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tonic::Request;
use tvos_net_player_cache_server::{
    AppState,
    config::CacheServerOptions,
    generated::tvos_net_player::v1::{
        BilibiliCredentialStatus, BilibiliPlaybackOptions, BilibiliResolveResult,
        BilibiliResolvedCandidate, BilibiliTaskResultItem, BilibiliTaskSelection,
        CreateBilibiliPlaybackTaskRequest, GetBilibiliCredentialStatusRequest, GetTaskRequest,
        PlaybackProtocol, PlaybackSource, ResolveBilibiliInputRequest, Task, TaskState,
        server_service_client::ServerServiceClient, task_service_client::TaskServiceClient,
    },
    run_grpc_listener, run_media_listener,
};

const BILIBILI_TASK_SELECTION_MODE_SINGLE: i32 = 3;
const BILIBILI_TASK_SELECTION_MODE_MULTIPLE: i32 = 4;
const BILIBILI_TASK_SELECTION_MODE_RANGE: i32 = 5;
const BILIBILI_TASK_SELECTION_MODE_ALL: i32 = 6;
const LIVE_CASE_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(60);
const BILIBILI_FAILURE_CLASS_TAG: &str = "bilibili_failure_class";
const CREDENTIAL_SAFE_CLIENT_DETAIL: &str =
    "Bilibili error detail omitted because credential material is configured.";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires live Bilibili network access and is intentionally outside default CI"]
async fn bilibili_live_cases_resolve_and_create_playable_hls() {
    let fixture_set = LiveFixtureSet::load();
    let run_policy = LiveRunPolicy::from_env();
    let http = reqwest::Client::new();
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
        let server = LiveTestServer::start().await;
        let task_tracker = LiveTaskTracker::default();
        let outcome = AssertUnwindSafe(async {
            let credential_status = fetch_bilibili_credential_status(server.channel().await).await;
            run_live_case(
                case,
                server.channel().await,
                &http,
                &server.media_url,
                Some(&credential_status),
                &task_tracker,
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

async fn run_live_case(
    case: &LiveCase,
    channel: tonic::transport::Channel,
    http: &reqwest::Client,
    media_url: &str,
    credential_status: Option<&BilibiliCredentialStatus>,
    task_tracker: &LiveTaskTracker,
) {
    assert_authenticated_case_ready(case, credential_status);

    let mut task_client = TaskServiceClient::new(channel);
    let options = case.playback_options.to_proto();
    let source = case.source();

    let resolved = task_client
        .resolve_bilibili_input(Request::new(ResolveBilibiliInputRequest {
            url_or_id: source.clone(),
            options: Some(options.clone()),
        }))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}",
                live_failure_message(case, "resolve", &error.to_string(), credential_status)
            )
        })
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
    if resolved.candidates.len() < case.minimum_candidates {
        panic!(
            "{}",
            live_failure_message(
                case,
                "resolve candidates",
                &format!(
                    "expected at least {} candidates, got {}",
                    case.minimum_candidates,
                    resolved.candidates.len()
                ),
                credential_status,
            )
        );
    }
    assert_resolved_candidate_contract(case, &resolved);

    let selection = case.selection_request(&resolved);
    let created = task_client
        .create_bilibili_playback_task(Request::new(CreateBilibiliPlaybackTaskRequest {
            url_or_id: source,
            options: Some(options),
            selection_id: selection.legacy_selection_id.clone(),
            selection: Some(selection.selection.clone()),
        }))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}",
                live_failure_message(
                    case,
                    "create playback task",
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
    #[serde(default)]
    expected_candidates_truncated: Option<bool>,
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

    fn selection_request(&self, resolved: &BilibiliResolveResult) -> LiveSelectionRequest {
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
    expected_result_items: usize,
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
            expected_result_items: 1,
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
            expected_result_items: expected_playable_results,
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
            expected_result_items: expected_playable_results,
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

fn first_candidate_id(case: &LiveCase, resolved: &BilibiliResolveResult) -> String {
    resolved
        .candidates
        .first()
        .unwrap_or_else(|| panic!("{}: resolved no selectable candidates", case.id))
        .selection_id
        .clone()
}

fn first_candidate_ids(
    case: &LiveCase,
    resolved: &BilibiliResolveResult,
    count: usize,
) -> Vec<String> {
    first_candidates(case, resolved, count)
        .into_iter()
        .map(|candidate| candidate.selection_id.clone())
        .collect()
}

fn first_candidates<'a>(
    case: &LiveCase,
    resolved: &'a BilibiliResolveResult,
    count: usize,
) -> Vec<&'a BilibiliResolvedCandidate> {
    assert!(
        resolved.candidates.len() >= count,
        "{}: expected at least {} candidates, got {}",
        case.id,
        count,
        resolved.candidates.len()
    );
    resolved.candidates.iter().take(count).collect()
}

fn assert_resolved_candidate_contract(case: &LiveCase, resolved: &BilibiliResolveResult) {
    if let Some(expected) = case.expected_candidates_truncated {
        assert_eq!(
            expected, resolved.candidates_truncated,
            "{}: unexpected candidates_truncated value",
            case.id
        );
    }

    if let Some(expected_source_kind) = case.expected_candidate_source_kind.as_deref() {
        for candidate in &resolved.candidates {
            assert_eq!(
                expected_source_kind, candidate.source_kind,
                "{}: candidate {} has unexpected source kind",
                case.id, candidate.selection_id
            );
        }
    }

    if case.requires_stable_item_selection {
        for candidate in &resolved.candidates {
            assert_stable_item_candidate(case, candidate);
        }
    }
}

fn assert_stable_item_candidate(case: &LiveCase, candidate: &BilibiliResolvedCandidate) {
    let selection_id = candidate.selection_id.as_str();
    assert!(
        selection_id.starts_with("item:"),
        "{}: candidate selection id is not a stable collection item id: {}",
        case.id,
        selection_id
    );
    assert!(
        selection_id.contains(":source:"),
        "{}: stable collection item selection lacks source binding: {}",
        case.id,
        selection_id
    );
    assert!(
        selection_id.contains(":cid:"),
        "{}: stable collection item id is missing cid: {}",
        case.id,
        selection_id
    );
    assert!(
        selection_id.contains(":aid:") || selection_id.contains(":bvid:"),
        "{}: stable collection item id is missing video identity: {}",
        case.id,
        selection_id
    );
    assert!(
        (1..=100).contains(&candidate.index),
        "{}: stable collection item index is outside the bounded candidate window: {}",
        case.id,
        candidate.index
    );
    assert!(
        !candidate.content_id.trim().is_empty(),
        "{}: stable collection item candidate has empty content id",
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
}

impl LiveRunPolicy {
    fn from_env() -> Self {
        Self {
            filter: case_filter_from_env(),
            include_authenticated: env_flag("BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED"),
            include_collection_list: env_flag("BILIBILI_LIVE_E2E_INCLUDE_COLLECTION_LIST"),
        }
    }

    fn run_decision(&self, case: &LiveCase) -> LiveRunDecision {
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
    env::var(key)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
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
    grpc_task: Option<JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
    media_task: Option<JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
}

impl LiveTestServer {
    async fn start() -> Self {
        let temp_root = tempfile::tempdir().unwrap();
        let root_path = temp_root.path().canonicalize().unwrap();
        let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_url = format!("http://{}", grpc_listener.local_addr().unwrap());
        let media_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let media_url = format!("http://{}", media_listener.local_addr().unwrap());
        let options = live_server_options(root_path.clone(), grpc_url.clone(), media_url.clone());
        let state = AppState::new(options);

        let grpc_task = tokio::spawn(run_grpc_listener(grpc_listener, state.clone()));
        let media_task = tokio::spawn(run_media_listener(media_listener, state.clone()));

        wait_for_grpc(&grpc_url).await;
        Self {
            temp_root: Some(temp_root),
            state,
            grpc_url,
            media_url,
            grpc_task: Some(grpc_task),
            media_task: Some(media_task),
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
        let background_result = self.cancel_case_tasks_and_wait(task_tracker).await;
        self.finish_teardown(combine_teardown_results(listener_result, background_result))
    }

    fn temp_root_path(&self) -> &Path {
        self.temp_root
            .as_ref()
            .expect("live e2e temp root should be present")
            .path()
    }

    fn finish_teardown(mut self, result: Result<(), String>) -> Result<(), String> {
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
                    self.state.shutdown_hls_fill_worker().await;
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
    args.extend(live_server_environment_args(|key| env::var(key).ok()));

    CacheServerOptions::from_args(args)
        .expect("live e2e cache server options should parse")
        .normalized_for_runtime()
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
        let valid = BilibiliResolvedCandidate {
            selection_id: "item:1:source:space-videos-123:cid:270001:bvid:BV1xx411c7mD:aid:170001"
                .to_owned(),
            source_kind: "space".to_owned(),
            content_id: "BV1xx411c7mD".to_owned(),
            index: 1,
            ..Default::default()
        };

        assert_stable_item_candidate(&case, &valid);
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
        };
        let mut case = test_case("space-collection", false, false);
        case.requires_collection_list_validation = true;

        assert_eq!(policy.run_decision(&case), LiveRunDecision::Run);
    }

    #[test]
    fn run_policy_still_skips_restricted_cases_by_default() {
        let policy = LiveRunPolicy::default();
        let case = test_case("bangumi-media-series", false, true);

        assert_eq!(
            policy.run_decision(&case),
            LiveRunDecision::Skip("requires explicit restricted-area live validation")
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
            expected_candidates_truncated: None,
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
