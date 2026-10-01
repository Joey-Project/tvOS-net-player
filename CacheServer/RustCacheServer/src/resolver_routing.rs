use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    resolver_catalog::Region,
    resolver_health::{ProbeFailure, ResolverHealthTracker},
    resolver_settings::{ResolverCandidate, normalize_resolver_host_id},
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::sleep,
};

const MAX_ROUTED_CANDIDATES: usize = 3;
const MAX_CONCURRENT_ROUTE_PLANS: usize = 2;
const PROBE_TOTAL_BUDGET: Duration = Duration::from_secs(2);
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_MEMORY_RECORDS: usize = 512;
const MAX_MEMORY_FILE_BYTES: usize = 96 * 1024;
const ROUTE_MEMORY_TTL_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryScope {
    Content,
    Series,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RouteMemoryEntry {
    pub(crate) scope: MemoryScope,
    pub(crate) key: String,
    pub(crate) host_id: String,
    pub(crate) area: Region,
    pub(crate) updated_at: u64,
    pub(crate) expires_at: u64,
}

#[derive(Clone, Debug, Default)]
struct RouteMemory {
    records: Vec<RouteMemoryEntry>,
}

#[derive(Clone)]
pub(crate) struct ResolverRoutingState {
    health: Arc<Mutex<ResolverHealthTracker>>,
    memory: Arc<Mutex<RouteMemory>>,
    memory_path: Option<Arc<PathBuf>>,
    probe_client: reqwest::Client,
    route_permits: Arc<Semaphore>,
}

impl ResolverRoutingState {
    pub(crate) fn load(memory_path: impl Into<PathBuf>) -> Self {
        let path = memory_path.into();
        let (memory, persist_path) = match read_memory(&path) {
            Ok(memory) => (memory, Some(Arc::new(path))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (RouteMemory::default(), Some(Arc::new(path)))
            }
            Err(_) => (RouteMemory::default(), None),
        };
        Self {
            health: Arc::new(Mutex::new(ResolverHealthTracker::default())),
            memory: Arc::new(Mutex::new(memory)),
            memory_path: persist_path,
            probe_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(800))
                .timeout(Duration::from_secs(1))
                .user_agent("tvOS-net-player-resolver-probe/1")
                .build()
                .expect("resolver probe client configuration should be valid"),
            route_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_ROUTE_PLANS)),
        }
    }

    pub(crate) async fn acquire_route_permit(&self) -> Result<OwnedSemaphorePermit, ()> {
        Arc::clone(&self.route_permits)
            .acquire_owned()
            .await
            .map_err(|_| ())
    }

    pub(crate) fn lookup(
        &self,
        content_key: &str,
        series_keys: &[String],
    ) -> Option<RouteMemoryEntry> {
        let now = unix_time_seconds();
        let mut memory = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        memory.records.retain(|entry| entry.expires_at > now);
        memory
            .records
            .iter()
            .find(|entry| entry.scope == MemoryScope::Content && entry.key == content_key)
            .or_else(|| {
                series_keys.iter().find_map(|key| {
                    memory
                        .records
                        .iter()
                        .find(|entry| entry.scope == MemoryScope::Series && entry.key == *key)
                })
            })
            .cloned()
    }

    pub(crate) fn record_region_route(
        &self,
        content_key: &str,
        series_keys: &[String],
        host_id: &str,
        area: Region,
    ) {
        let Ok(host_id) = normalize_resolver_host_id(host_id) else {
            return;
        };
        if area == Region::All {
            return;
        }
        let now = unix_time_seconds();
        let expiry = now.saturating_add(ROUTE_MEMORY_TTL_SECS);
        let mut records = Vec::with_capacity(3);
        if valid_memory_key(MemoryScope::Content, content_key) {
            records.push(RouteMemoryEntry {
                scope: MemoryScope::Content,
                key: content_key.to_owned(),
                host_id: host_id.clone(),
                area,
                updated_at: now,
                expires_at: expiry,
            });
        }
        for key in series_keys
            .iter()
            .filter(|key| valid_memory_key(MemoryScope::Series, key))
        {
            records.push(RouteMemoryEntry {
                scope: MemoryScope::Series,
                key: key.clone(),
                host_id: host_id.clone(),
                area,
                updated_at: now,
                expires_at: expiry,
            });
        }
        if records.is_empty() {
            return;
        }
        let mut memory = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for record in records {
            memory
                .records
                .retain(|existing| existing.scope != record.scope || existing.key != record.key);
            memory.records.push(record);
        }
        memory.records.sort_by_key(|entry| entry.updated_at);
        if memory.records.len() > MAX_MEMORY_RECORDS {
            let excess = memory.records.len() - MAX_MEMORY_RECORDS;
            memory.records.drain(..excess);
        }
        if let Some(path) = self.memory_path.as_deref() {
            let _ = persist_memory(path, &memory);
        }
    }

    pub(crate) async fn probe_candidates(
        &self,
        candidates: &[ResolverCandidate],
        area: Region,
        preferred_host_id: Option<&str>,
        is_cancel_requested: impl Fn() -> bool,
    ) -> Result<Vec<ResolverCandidate>, ()> {
        let mut eligible = candidates
            .iter()
            .filter(|candidate| candidate_supports_area(candidate, area))
            .cloned()
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            return Ok(eligible);
        }

        let now = Instant::now();
        let mut ids = eligible
            .iter()
            .map(|candidate| candidate.host_id.clone())
            .collect::<Vec<_>>();
        let (ranked_ids, cached_healthy, due) = {
            let mut health = self
                .health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            health.rank_candidates(&mut ids, now);
            let cached = eligible
                .iter()
                .filter(|candidate| health.recently_healthy(&candidate.host_id, now))
                .map(|candidate| candidate.host_id.clone())
                .collect::<HashSet<_>>();
            let due = eligible
                .iter()
                .filter(|candidate| health.probe_due(&candidate.host_id, now))
                .map(|candidate| candidate.host_id.clone())
                .collect::<HashSet<_>>();
            (ids, cached, due)
        };
        let ranks = ranked_ids
            .iter()
            .enumerate()
            .map(|(rank, id)| (id.as_str(), rank))
            .collect::<HashMap<_, _>>();
        eligible.sort_by_key(|candidate| {
            ranks
                .get(candidate.host_id.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
        if let Some(preferred) = preferred_host_id
            && let Some(index) = eligible
                .iter()
                .position(|candidate| candidate.host_id == preferred)
        {
            let preferred = eligible.remove(index);
            eligible.insert(0, preferred);
        }
        bound_route_candidates(&mut eligible);

        let mut reachable = eligible
            .iter()
            .filter(|candidate| cached_healthy.contains(&candidate.host_id))
            .map(|candidate| candidate.host_id.clone())
            .collect::<HashSet<_>>();
        let mut pending = JoinSet::new();
        for candidate in eligible
            .iter()
            .filter(|candidate| due.contains(&candidate.host_id))
        {
            let client = self.probe_client.clone();
            let candidate = candidate.clone();
            pending.spawn(async move { probe_one(client, candidate).await });
        }

        let deadline = tokio::time::Instant::now() + PROBE_TOTAL_BUDGET;
        while !pending.is_empty() {
            tokio::select! {
                joined = pending.join_next() => {
                    let Some(Ok((candidate, elapsed, outcome))) = joined else { continue };
                    let mut health = self.health.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match outcome {
                        ProbeOutcome::Ready => {
                            health.record_success(&candidate.host_id, Instant::now(), elapsed);
                            reachable.insert(candidate.host_id);
                        }
                        ProbeOutcome::Failure(failure) => {
                            health.record_failure(&candidate.host_id, Instant::now(), failure);
                        }
                        ProbeOutcome::Neutral => {
                            health.record_neutral_probe(&candidate.host_id, Instant::now());
                        }
                    }
                }
                () = sleep(PROBE_POLL_INTERVAL) => {
                    if is_cancel_requested() {
                        pending.abort_all();
                        return Err(());
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    pending.abort_all();
                    break;
                }
            }
        }

        if is_cancel_requested() {
            return Err(());
        }
        Ok(eligible
            .into_iter()
            .filter(|candidate| reachable.contains(&candidate.host_id))
            .collect())
    }
}

#[derive(Debug)]
enum ProbeOutcome {
    Ready,
    Failure(ProbeFailure),
    Neutral,
}

async fn probe_one(
    client: reqwest::Client,
    candidate: ResolverCandidate,
) -> (ResolverCandidate, Duration, ProbeOutcome) {
    let started = Instant::now();
    let result = client.get(&candidate.origin).send().await;
    let elapsed = started.elapsed();
    let outcome = match result {
        Ok(response) if response.status().is_redirection() => ProbeOutcome::Neutral,
        Ok(response) if response.status().is_server_error() => ProbeOutcome::Neutral,
        Ok(_) => ProbeOutcome::Ready,
        Err(error) if error.is_timeout() => ProbeOutcome::Failure(ProbeFailure::Timeout),
        Err(error) if error.is_connect() => ProbeOutcome::Failure(ProbeFailure::Connectivity),
        Err(_) => ProbeOutcome::Failure(ProbeFailure::Connectivity),
    };
    (candidate, elapsed, outcome)
}

fn candidate_supports_area(candidate: &ResolverCandidate, area: Region) -> bool {
    candidate.regions.contains(&Region::All) || candidate.regions.contains(&area)
}

fn bound_route_candidates(candidates: &mut Vec<ResolverCandidate>) {
    candidates.truncate(MAX_ROUTED_CANDIDATES);
}

pub(crate) fn infer_region(text: &str) -> Option<Region> {
    let text = text.to_lowercase();
    if ["香港", "港澳", "hong kong", "hongkong"]
        .iter()
        .any(|hint| text.contains(hint))
    {
        Some(Region::Hk)
    } else if ["台湾", "台灣", "taiwan", "taipei"]
        .iter()
        .any(|hint| text.contains(hint))
    {
        Some(Region::Tw)
    } else if ["泰国", "泰國", "thailand"]
        .iter()
        .any(|hint| text.contains(hint))
    {
        Some(Region::Th)
    } else if ["中国大陆", "中國大陸", "mainland china"]
        .iter()
        .any(|hint| text.contains(hint))
    {
        Some(Region::Cn)
    } else {
        None
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedRouteMemory {
    version: u32,
    records: Vec<PersistedRouteMemoryEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedRouteMemoryEntry {
    scope: MemoryScopeWire,
    key: String,
    host_id: String,
    area: Region,
    updated_at: u64,
    expires_at: u64,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemoryScopeWire {
    Content,
    Series,
}

fn read_memory(path: &Path) -> io::Result<RouteMemory> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_MEMORY_FILE_BYTES as u64 {
        return Err(invalid_memory_file());
    }
    let file = nofollow_open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_MEMORY_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MEMORY_FILE_BYTES {
        return Err(invalid_memory_file());
    }
    let persisted: PersistedRouteMemory =
        serde_json::from_slice(&bytes).map_err(|_| invalid_memory_file())?;
    if persisted.version != 1 || persisted.records.len() > MAX_MEMORY_RECORDS {
        return Err(invalid_memory_file());
    }
    let mut records = Vec::with_capacity(persisted.records.len());
    for entry in persisted.records {
        let scope = match entry.scope {
            MemoryScopeWire::Content => MemoryScope::Content,
            MemoryScopeWire::Series => MemoryScope::Series,
        };
        let host_id =
            normalize_resolver_host_id(&entry.host_id).map_err(|_| invalid_memory_file())?;
        if !valid_memory_key(scope, &entry.key)
            || entry.area == Region::All
            || entry.expires_at <= entry.updated_at
        {
            return Err(invalid_memory_file());
        }
        records.push(RouteMemoryEntry {
            scope,
            key: entry.key,
            host_id,
            area: entry.area,
            updated_at: entry.updated_at,
            expires_at: entry.expires_at,
        });
    }
    let now = unix_time_seconds();
    records.retain(|entry| entry.expires_at > now);
    Ok(RouteMemory { records })
}

fn persist_memory(path: &Path, memory: &RouteMemory) -> io::Result<()> {
    if memory.records.len() > MAX_MEMORY_RECORDS
        || memory.records.iter().any(|entry| {
            !valid_memory_key(entry.scope, &entry.key)
                || normalize_resolver_host_id(&entry.host_id).is_err()
                || entry.area == Region::All
                || entry.expires_at <= entry.updated_at
        })
    {
        return Err(invalid_memory_file());
    }
    let parent = path.parent().ok_or_else(invalid_memory_file)?;
    fs::create_dir_all(parent).map_err(|_| invalid_memory_file())?;
    if !fs::symlink_metadata(parent)
        .map_err(|_| invalid_memory_file())?
        .file_type()
        .is_dir()
    {
        return Err(invalid_memory_file());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => return Err(invalid_memory_file()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(invalid_memory_file()),
    }
    let entries = memory
        .records
        .iter()
        .map(|entry| PersistedRouteMemoryEntry {
            scope: match entry.scope {
                MemoryScope::Content => MemoryScopeWire::Content,
                MemoryScope::Series => MemoryScopeWire::Series,
            },
            key: entry.key.clone(),
            host_id: entry.host_id.clone(),
            area: entry.area,
            updated_at: entry.updated_at,
            expires_at: entry.expires_at,
        })
        .collect();
    let bytes = serde_json::to_vec(&PersistedRouteMemory {
        version: 1,
        records: entries,
    })
    .map_err(|_| invalid_memory_file())?;
    if bytes.len() > MAX_MEMORY_FILE_BYTES {
        return Err(invalid_memory_file());
    }
    let temporary = parent.join(format!(".resolver-routing-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = create_private_file(&temporary)?;
        file.write_all(&bytes).map_err(|_| invalid_memory_file())?;
        file.sync_all().map_err(|_| invalid_memory_file())?;
        fs::rename(&temporary, path).map_err(|_| invalid_memory_file())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn valid_memory_key(scope: MemoryScope, key: &str) -> bool {
    if key.len() > 64 {
        return false;
    }
    let (prefix, number) = match scope {
        MemoryScope::Content => key.strip_prefix("epid:").map(|value| ("epid:", value)),
        MemoryScope::Series => key
            .strip_prefix("season:")
            .map(|value| ("season:", value))
            .or_else(|| key.strip_prefix("media:").map(|value| ("media:", value))),
    }
    .unwrap_or(("", ""));
    !prefix.is_empty() && !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn unix_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn invalid_memory_file() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid resolver memory file")
}

#[cfg(unix)]
fn nofollow_open(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn nofollow_open(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_MEMORY_RECORDS, MAX_ROUTED_CANDIDATES, MemoryScope, ResolverRoutingState, RouteMemory,
        RouteMemoryEntry, infer_region, persist_memory, read_memory,
    };
    use crate::{resolver_catalog::Region, resolver_settings::ResolverCandidate};
    use std::{
        fs,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    fn candidate(host_id: &str, regions: Vec<Region>) -> ResolverCandidate {
        ResolverCandidate {
            host_id: host_id.to_owned(),
            origin: format!("https://{host_id}/"),
            name: host_id.to_owned(),
            regions,
        }
    }

    #[test]
    fn title_hints_map_only_supported_pgc_regions() {
        assert_eq!(infer_region("香港限定"), Some(Region::Hk));
        assert_eq!(infer_region("Taiwan release"), Some(Region::Tw));
        assert_eq!(infer_region("泰国版"), Some(Region::Th));
        assert_eq!(infer_region("普通国创"), None);
    }

    #[test]
    fn memory_is_bounded_expires_and_prefers_content_over_series() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resolver-routing-memory.json");
        let state = ResolverRoutingState::load(&path);
        let series_keys = vec!["season:20".to_owned()];
        state.record_region_route("epid:10", &series_keys, "proxy.example", Region::Hk);
        assert_eq!(
            state.lookup("epid:10", &series_keys).unwrap().scope,
            MemoryScope::Content
        );
        state.record_region_route("epid:11", &series_keys, "other.example", Region::Tw);
        assert_eq!(
            state.lookup("epid:12", &series_keys).unwrap().host_id,
            "other.example"
        );

        let memory = RouteMemory {
            records: (0..=MAX_MEMORY_RECORDS)
                .map(|index| RouteMemoryEntry {
                    scope: MemoryScope::Content,
                    key: format!("epid:{index}"),
                    host_id: "proxy.example".to_owned(),
                    area: Region::Hk,
                    updated_at: index as u64,
                    expires_at: index as u64 + Duration::from_secs(10).as_secs(),
                })
                .collect(),
        };
        assert!(persist_memory(&path, &memory).is_err());
    }

    #[test]
    fn persisted_memory_contains_only_bounded_identity_and_host_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resolver-routing-memory.json");
        let mut memory = RouteMemory::default();
        memory.records.push(RouteMemoryEntry {
            scope: MemoryScope::Content,
            key: "epid:123".to_owned(),
            host_id: "proxy.example".to_owned(),
            area: Region::Hk,
            updated_at: unix_now(),
            expires_at: unix_now() + 20,
        });
        persist_memory(&path, &memory).unwrap();
        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("proxy.example"));
        for secret in ["signed", "token", "cookie", "access_key", "https://"] {
            assert!(!text.contains(secret));
        }
        assert_eq!(read_memory(&path).unwrap().records, memory.records);
    }

    #[test]
    fn custom_port_affinity_survives_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resolver-routing-memory.json");
        let state = ResolverRoutingState::load(&path);
        let series_keys = vec!["season:789".to_owned(), "media:7890".to_owned()];
        state.record_region_route("epid:456", &series_keys, "custom.example:8443", Region::Cn);
        let restarted = ResolverRoutingState::load(&path);
        let route = restarted.lookup("epid:456", &series_keys).unwrap();
        assert_eq!(route.host_id, "custom.example:8443");
        assert_eq!(route.area, Region::Cn);
        let media_only = restarted
            .lookup("epid:999", &["media:7890".to_owned()])
            .unwrap();
        assert_eq!(media_only.host_id, "custom.example:8443");
    }

    #[test]
    fn region_filter_excludes_candidates_not_enabled_for_area() {
        let candidates = [
            candidate("all.example", vec![Region::All]),
            candidate("hk.example", vec![Region::Hk]),
            candidate("tw.example", vec![Region::Tw]),
        ];
        assert!(super::candidate_supports_area(&candidates[0], Region::Hk));
        assert!(super::candidate_supports_area(&candidates[1], Region::Hk));
        assert!(!super::candidate_supports_area(&candidates[2], Region::Hk));
    }

    #[test]
    fn each_pgc_route_is_limited_to_three_ranked_candidates() {
        let mut candidates = (0..6)
            .map(|index| candidate(&format!("proxy-{index}.example"), vec![Region::Cn]))
            .collect::<Vec<_>>();
        super::bound_route_candidates(&mut candidates);
        assert_eq!(candidates.len(), MAX_ROUTED_CANDIDATES);
        assert_eq!(candidates[0].host_id, "proxy-0.example");
        assert_eq!(candidates[2].host_id, "proxy-2.example");
    }

    #[tokio::test]
    async fn route_admission_queues_instead_of_dropping_settings_routing() {
        let temp = tempfile::tempdir().unwrap();
        let state = ResolverRoutingState::load(temp.path().join("resolver-memory.json"));
        let first = state.acquire_route_permit().await.unwrap();
        let second = state.acquire_route_permit().await.unwrap();
        let queued_state = state.clone();
        let mut waiter =
            tokio::spawn(async move { queued_state.acquire_route_permit().await.unwrap() });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err()
        );
        drop(first);
        let third = tokio::time::timeout(Duration::from_millis(200), &mut waiter)
            .await
            .unwrap()
            .unwrap();
        drop((second, third));
    }

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}
