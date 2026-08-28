use std::{
    cell::Cell,
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[cfg(test)]
use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};

use prost_types::Timestamp;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, SeqAccess, Visitor},
};

use crate::generated::tvos_net_player::v1::{
    BilibiliDownloadOptions, BilibiliPlaybackOptions, BilibiliPlaybackSession,
    BilibiliPlaybackVariant, BilibiliTaskResultItem, BilibiliTaskSelection, CacheResourceRef,
    LanTranscodingPlan, PlaybackSource, Task, TaskArtifact, TaskProblem, TaskResult,
    TaskResultProgress,
};
use crate::playback_policy::PlaybackPolicy;
use crate::task_output::{
    MAX_REGISTERED_TASK_RESOURCES, MAX_TASK_ARTIFACTS, MAX_TASK_RESOURCES, MAX_TASK_RESULTS,
    TaskOutputRecord, TaskResourceRecord,
};

const LEGACY_TASK_STATE_SCHEMA_VERSION: u32 = 1;
const TASK_STATE_SCHEMA_VERSION: u32 = 2;
const MAX_TASK_STATE_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
const MAX_PERSISTED_TASKS: usize = 10_000;
const MAX_PERSISTED_BILIBILI_VARIANTS: usize = 10_000;
const MAX_PERSISTED_DANMAKU_FORMATS: usize = 16;

thread_local! {
    static PERSISTED_TASK_RESOURCE_BUDGET: Cell<Option<PersistedTaskResourceBudget>> = const { Cell::new(None) };
    static PERSISTED_TASK_ARTIFACT_BUDGET: Cell<Option<PersistedTaskArtifactBudget>> = const { Cell::new(None) };
}

#[derive(Clone, Copy)]
struct PersistedTaskResourceBudget {
    remaining: usize,
    limit: usize,
}

struct PersistedTaskResourceBudgetGuard;

impl PersistedTaskResourceBudgetGuard {
    fn enter(limit: usize) -> io::Result<Self> {
        let activated = PERSISTED_TASK_RESOURCE_BUDGET.with(|budget| {
            if budget.get().is_some() {
                return false;
            }
            budget.set(Some(PersistedTaskResourceBudget {
                remaining: limit,
                limit,
            }));
            true
        });
        if !activated {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state resource deserialization is already active",
            ));
        }
        Ok(Self)
    }
}

impl Drop for PersistedTaskResourceBudgetGuard {
    fn drop(&mut self) {
        PERSISTED_TASK_RESOURCE_BUDGET.with(|budget| budget.set(None));
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PersistedTaskResourceBudgetClaim {
    Unbounded,
    Claimed,
    Exhausted,
}

fn claim_persisted_task_resource_budget() -> PersistedTaskResourceBudgetClaim {
    PERSISTED_TASK_RESOURCE_BUDGET.with(|budget| match budget.get() {
        None => PersistedTaskResourceBudgetClaim::Unbounded,
        Some(state) if state.remaining == 0 => PersistedTaskResourceBudgetClaim::Exhausted,
        Some(mut state) => {
            state.remaining -= 1;
            budget.set(Some(state));
            PersistedTaskResourceBudgetClaim::Claimed
        }
    })
}

fn release_persisted_task_resource_budget(claim: PersistedTaskResourceBudgetClaim) {
    if claim != PersistedTaskResourceBudgetClaim::Claimed {
        return;
    }
    PERSISTED_TASK_RESOURCE_BUDGET.with(|budget| {
        let mut state = budget
            .get()
            .expect("claimed task resource budget must remain active");
        state.remaining = state.remaining.saturating_add(1).min(state.limit);
        budget.set(Some(state));
    });
}

fn persisted_task_resource_capacity(size_hint: Option<usize>) -> usize {
    let remaining = PERSISTED_TASK_RESOURCE_BUDGET
        .with(|budget| budget.get())
        .map(|budget| budget.remaining)
        .unwrap_or(MAX_TASK_RESOURCES);
    size_hint
        .unwrap_or(0)
        .min(MAX_TASK_RESOURCES)
        .min(remaining)
}

fn persisted_task_resource_limit() -> usize {
    PERSISTED_TASK_RESOURCE_BUDGET
        .with(|budget| budget.get())
        .map(|budget| budget.limit)
        .unwrap_or(MAX_REGISTERED_TASK_RESOURCES)
}

#[derive(Clone, Copy)]
struct PersistedTaskArtifactBudget {
    remaining: usize,
    limit: usize,
}

struct PersistedTaskArtifactBudgetGuard;

impl PersistedTaskArtifactBudgetGuard {
    fn enter(limit: usize) -> io::Result<Self> {
        let activated = PERSISTED_TASK_ARTIFACT_BUDGET.with(|budget| {
            if budget.get().is_some() {
                return false;
            }
            budget.set(Some(PersistedTaskArtifactBudget {
                remaining: limit,
                limit,
            }));
            true
        });
        if !activated {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task output artifact deserialization is already active",
            ));
        }
        Ok(Self)
    }
}

impl Drop for PersistedTaskArtifactBudgetGuard {
    fn drop(&mut self) {
        PERSISTED_TASK_ARTIFACT_BUDGET.with(|budget| budget.set(None));
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PersistedTaskArtifactBudgetClaim {
    Unbounded,
    Claimed,
    Exhausted,
}

fn claim_persisted_task_artifact_budget() -> PersistedTaskArtifactBudgetClaim {
    PERSISTED_TASK_ARTIFACT_BUDGET.with(|budget| match budget.get() {
        None => PersistedTaskArtifactBudgetClaim::Unbounded,
        Some(state) if state.remaining == 0 => PersistedTaskArtifactBudgetClaim::Exhausted,
        Some(mut state) => {
            state.remaining -= 1;
            budget.set(Some(state));
            PersistedTaskArtifactBudgetClaim::Claimed
        }
    })
}

fn release_persisted_task_artifact_budget(claim: PersistedTaskArtifactBudgetClaim) {
    if claim != PersistedTaskArtifactBudgetClaim::Claimed {
        return;
    }
    PERSISTED_TASK_ARTIFACT_BUDGET.with(|budget| {
        let mut state = budget
            .get()
            .expect("claimed task artifact budget must remain active");
        state.remaining = state.remaining.saturating_add(1).min(state.limit);
        budget.set(Some(state));
    });
}

fn persisted_task_artifact_capacity(size_hint: Option<usize>) -> usize {
    let remaining = PERSISTED_TASK_ARTIFACT_BUDGET
        .with(|budget| budget.get())
        .map(|budget| budget.remaining)
        .unwrap_or(MAX_TASK_ARTIFACTS);
    size_hint
        .unwrap_or(0)
        .min(MAX_TASK_ARTIFACTS)
        .min(remaining)
}

fn persisted_task_artifact_limit() -> usize {
    PERSISTED_TASK_ARTIFACT_BUDGET
        .with(|budget| budget.get())
        .map(|budget| budget.limit)
        .unwrap_or(MAX_TASK_ARTIFACTS)
}

#[cfg(test)]
type TaskStateSaveBarriers = (Arc<Barrier>, Arc<Barrier>);

#[derive(Clone)]
pub(crate) struct TaskStateStore {
    path: Arc<PathBuf>,
    pending_directory_syncs: Arc<Mutex<Vec<PathBuf>>>,
    #[cfg(test)]
    fail_next_directory_sync: Arc<AtomicBool>,
    #[cfg(test)]
    next_save_barriers: Arc<Mutex<Option<TaskStateSaveBarriers>>>,
}

#[derive(Debug)]
pub(crate) enum TaskStateSaveOutcome {
    Durable,
    InstalledButNotDurable(io::Error),
}

impl TaskStateStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        let path = Arc::new(path.into());
        let pending_directory_syncs = initial_parent_directories_requiring_sync(&path);
        Self {
            path,
            pending_directory_syncs: Arc::new(Mutex::new(pending_directory_syncs)),
            #[cfg(test)]
            fail_next_directory_sync: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            next_save_barriers: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> io::Result<Vec<PersistedTaskRecord>> {
        let file = match File::open(self.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_TASK_STATE_SNAPSHOT_BYTES as u64 {
            return Err(snapshot_size_error());
        }
        let mut bytes = Vec::new();
        file.take((MAX_TASK_STATE_SNAPSHOT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_TASK_STATE_SNAPSHOT_BYTES {
            return Err(snapshot_size_error());
        }
        let snapshot = deserialize_task_snapshot(&bytes)?;
        if !matches!(
            snapshot.schema_version,
            LEGACY_TASK_STATE_SCHEMA_VERSION | TASK_STATE_SCHEMA_VERSION
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported task state schema version: {}",
                    snapshot.schema_version
                ),
            ));
        }

        let schema_version = snapshot.schema_version;
        let records = snapshot
            .tasks
            .into_iter()
            .map(|file| file.into_record(schema_version))
            .collect::<io::Result<Vec<_>>>()?;
        validate_registered_task_resource_count(&records)?;
        Ok(records)
    }

    pub(crate) fn save(&self, records: &[PersistedTaskRecord]) -> io::Result<TaskStateSaveOutcome> {
        validate_registered_task_resource_count(records)?;
        let snapshot = PersistedTaskSnapshot {
            schema_version: TASK_STATE_SCHEMA_VERSION,
            tasks: records
                .iter()
                .cloned()
                .map(PersistedTaskFile::from)
                .collect(),
        };
        snapshot.validate_collection_limits()?;
        let directories_to_sync = parent_directories_requiring_sync(self.path())?;
        self.remember_pending_directory_syncs(&directories_to_sync);
        create_parent_directory(self.path())?;
        #[cfg(test)]
        if let Some((entered, resume)) = self
            .next_save_barriers
            .lock()
            .expect("task state save barrier lock poisoned")
            .take()
        {
            entered.wait();
            resume.wait();
        }

        let mut serialized = BoundedSnapshotWriter::new(MAX_TASK_STATE_SNAPSHOT_BYTES);
        serde_json::to_writer_pretty(&mut serialized, &snapshot).map_err(invalid_data)?;
        serialized.write_all(b"\n")?;
        let bytes = serialized.into_inner();
        let temp_path = temp_path_for(self.path());
        let mut temp_file = File::create(&temp_path)?;
        temp_file.write_all(&bytes)?;
        temp_file.sync_all()?;
        drop(temp_file);
        fs::rename(temp_path, self.path())?;
        #[cfg(test)]
        if self
            .fail_next_directory_sync
            .swap(false, AtomicOrdering::AcqRel)
        {
            return Ok(TaskStateSaveOutcome::InstalledButNotDurable(
                io::Error::other("injected task state directory sync failure"),
            ));
        }
        let mut pending_directory_syncs = self
            .pending_directory_syncs
            .lock()
            .expect("task state directory sync lock poisoned");
        match sync_directories(&pending_directory_syncs) {
            Ok(()) => {
                pending_directory_syncs.clear();
                Ok(TaskStateSaveOutcome::Durable)
            }
            Err(error) => Ok(TaskStateSaveOutcome::InstalledButNotDurable(error)),
        }
    }

    fn remember_pending_directory_syncs(&self, directories: &[PathBuf]) {
        let mut pending = self
            .pending_directory_syncs
            .lock()
            .expect("task state directory sync lock poisoned");
        for directory in directories {
            if !pending.contains(directory) {
                pending.push(directory.clone());
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_directory_sync(&self) {
        self.fail_next_directory_sync
            .store(true, AtomicOrdering::Release);
    }

    #[cfg(test)]
    pub(crate) fn block_next_save(&self, entered: Arc<Barrier>, resume: Arc<Barrier>) {
        *self
            .next_save_barriers
            .lock()
            .expect("task state save barrier lock poisoned") = Some((entered, resume));
    }

    #[cfg(test)]
    fn pending_directory_syncs(&self) -> Vec<PathBuf> {
        self.pending_directory_syncs
            .lock()
            .expect("task state directory sync lock poisoned")
            .clone()
    }
}

fn deserialize_task_snapshot(bytes: &[u8]) -> io::Result<PersistedTaskSnapshot> {
    deserialize_task_snapshot_with_resource_limit(bytes, MAX_REGISTERED_TASK_RESOURCES)
}

fn deserialize_task_snapshot_with_resource_limit(
    bytes: &[u8],
    resource_limit: usize,
) -> io::Result<PersistedTaskSnapshot> {
    let _budget = PersistedTaskResourceBudgetGuard::enter(resource_limit)?;
    serde_json::from_slice(bytes).map_err(invalid_data)
}

fn validate_registered_task_resource_count(records: &[PersistedTaskRecord]) -> io::Result<()> {
    let resource_count = records
        .iter()
        .try_fold(0_usize, |total, record| {
            total.checked_add(record.output.resources.len())
        })
        .unwrap_or(usize::MAX);
    if resource_count > MAX_REGISTERED_TASK_RESOURCES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "task state cannot contain more than {MAX_REGISTERED_TASK_RESOURCES} registered resources"
            ),
        ));
    }
    Ok(())
}

struct BoundedSnapshotWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedSnapshotWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedSnapshotWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(snapshot_size_error());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn snapshot_size_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("task state snapshot cannot exceed {MAX_TASK_STATE_SNAPSHOT_BYTES} bytes"),
    )
}

fn deserialize_bounded_vec<'de, D, T>(
    deserializer: D,
    limit: usize,
    label: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedVecVisitor<T> {
        limit: usize,
        label: &'static str,
        marker: PhantomData<fn() -> T>,
    }

    impl<'de, T> Visitor<'de> for BoundedVecVisitor<T>
    where
        T: Deserialize<'de>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "an array containing at most {} {}",
                self.limit, self.label
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(self.limit));
            while values.len() < self.limit {
                let Some(value) = sequence.next_element()? else {
                    return Ok(values);
                };
                values.push(value);
            }
            if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom(format!(
                    "{} cannot exceed {} entries",
                    self.label, self.limit
                )));
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedVecVisitor {
        limit,
        label,
        marker: PhantomData,
    })
}

fn deserialize_persisted_tasks<'de, D>(deserializer: D) -> Result<Vec<PersistedTaskFile>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_PERSISTED_TASKS, "persisted tasks")
}

fn deserialize_bilibili_result_items<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedBilibiliTaskResultItem>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_TASK_RESULTS, "Bilibili task result items")
}

fn deserialize_task_results<'de, D>(deserializer: D) -> Result<Vec<PersistedTaskResult>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_task_results_with_artifact_limit(deserializer, MAX_TASK_ARTIFACTS)
}

fn deserialize_task_results_with_artifact_limit<'de, D>(
    deserializer: D,
    artifact_limit: usize,
) -> Result<Vec<PersistedTaskResult>, D::Error>
where
    D: Deserializer<'de>,
{
    struct TaskResultsVisitor {
        artifact_limit: usize,
    }

    impl<'de> Visitor<'de> for TaskResultsVisitor {
        type Value = Vec<PersistedTaskResult>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "at most {MAX_TASK_RESULTS} task results containing at most {} artifacts",
                self.artifact_limit
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut results =
                Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX_TASK_RESULTS));
            while results.len() < MAX_TASK_RESULTS {
                let Some(result) = sequence.next_element::<PersistedTaskResult>()? else {
                    return Ok(results);
                };
                results.push(result);
            }
            if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom(format!(
                    "task output results cannot exceed {MAX_TASK_RESULTS} entries"
                )));
            }
            Ok(results)
        }
    }

    let _artifact_budget = PersistedTaskArtifactBudgetGuard::enter(artifact_limit)
        .map_err(<D::Error as de::Error>::custom)?;
    deserializer.deserialize_seq(TaskResultsVisitor { artifact_limit })
}

fn deserialize_task_resources<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedCacheResourceRef>, D::Error>
where
    D: Deserializer<'de>,
{
    struct TaskResourcesVisitor;

    impl<'de> Visitor<'de> for TaskResourcesVisitor {
        type Value = Vec<PersistedCacheResourceRef>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "at most {MAX_TASK_RESOURCES} task output resources within the snapshot-wide resource budget"
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut resources =
                Vec::with_capacity(persisted_task_resource_capacity(sequence.size_hint()));
            while resources.len() < MAX_TASK_RESOURCES {
                let claim = claim_persisted_task_resource_budget();
                if claim == PersistedTaskResourceBudgetClaim::Exhausted {
                    if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                        let limit = persisted_task_resource_limit();
                        return Err(de::Error::custom(format!(
                            "task state cannot contain more than {limit} registered resources"
                        )));
                    }
                    return Ok(resources);
                }
                let Some(resource) = sequence.next_element::<PersistedCacheResourceRef>()? else {
                    release_persisted_task_resource_budget(claim);
                    return Ok(resources);
                };
                resources.push(resource);
            }
            if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom(format!(
                    "task output resources cannot exceed {MAX_TASK_RESOURCES} entries"
                )));
            }
            Ok(resources)
        }
    }

    deserializer.deserialize_seq(TaskResourcesVisitor)
}

fn deserialize_task_artifacts<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedTaskArtifact>, D::Error>
where
    D: Deserializer<'de>,
{
    struct TaskArtifactsVisitor;

    impl<'de> Visitor<'de> for TaskArtifactsVisitor {
        type Value = Vec<PersistedTaskArtifact>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "at most {MAX_TASK_ARTIFACTS} task result artifacts within the task output artifact budget"
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut artifacts =
                Vec::with_capacity(persisted_task_artifact_capacity(sequence.size_hint()));
            while artifacts.len() < MAX_TASK_ARTIFACTS {
                let claim = claim_persisted_task_artifact_budget();
                if claim == PersistedTaskArtifactBudgetClaim::Exhausted {
                    if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                        let limit = persisted_task_artifact_limit();
                        return Err(de::Error::custom(format!(
                            "task output artifacts cannot exceed {limit} entries"
                        )));
                    }
                    return Ok(artifacts);
                }
                let Some(artifact) = sequence.next_element::<PersistedTaskArtifact>()? else {
                    release_persisted_task_artifact_budget(claim);
                    return Ok(artifacts);
                };
                artifacts.push(artifact);
            }
            if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom(format!(
                    "task result artifacts cannot exceed {MAX_TASK_ARTIFACTS} entries"
                )));
            }
            Ok(artifacts)
        }
    }

    deserializer.deserialize_seq(TaskArtifactsVisitor)
}

fn deserialize_selection_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_TASK_RESULTS, "Bilibili selection ids")
}

fn deserialize_playback_variants<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedBilibiliPlaybackVariant>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(
        deserializer,
        MAX_PERSISTED_BILIBILI_VARIANTS,
        "Bilibili playback variants",
    )
}

fn deserialize_danmaku_formats<'de, D>(deserializer: D) -> Result<Vec<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(
        deserializer,
        MAX_PERSISTED_DANMAKU_FORMATS,
        "Bilibili danmaku formats",
    )
}

#[cfg(unix)]
fn parent_directory_sync_chain(path: &Path) -> io::Result<Vec<PathBuf>> {
    let Some(parent) = path.parent() else {
        return Ok(Vec::new());
    };

    let mut directories = Vec::new();
    for ancestor in parent.ancestors() {
        let ancestor = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        };
        directories.push(ancestor.to_path_buf());

        match fs::metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() => return Ok(directories),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!(
                        "task state parent is not a directory: {}",
                        ancestor.display()
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "task state directory has no existing ancestor: {}",
            parent.display()
        ),
    ))
}

#[cfg(unix)]
fn initial_parent_directories_requiring_sync(path: &Path) -> Vec<PathBuf> {
    path.parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .map(|ancestor| {
            if ancestor.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                ancestor.to_path_buf()
            }
        })
        .collect()
}

#[cfg(not(unix))]
fn initial_parent_directories_requiring_sync(_path: &Path) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(unix)]
fn parent_directories_requiring_sync(path: &Path) -> io::Result<Vec<PathBuf>> {
    parent_directory_sync_chain(path)
}

#[cfg(not(unix))]
fn parent_directories_requiring_sync(_path: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(Vec::new())
}

fn create_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)
}

#[cfg(test)]
fn prepare_parent_directory(path: &Path) -> io::Result<Vec<PathBuf>> {
    let directories_to_sync = parent_directories_requiring_sync(path)?;
    create_parent_directory(path)?;
    Ok(directories_to_sync)
}

#[cfg(unix)]
fn sync_directories(directories: &[PathBuf]) -> io::Result<()> {
    for directory in directories {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_directories(_directories: &[PathBuf]) -> io::Result<()> {
    Ok(())
}

#[derive(Clone)]
pub(crate) struct PersistedTaskRecord {
    pub(crate) task: Task,
    pub(crate) options: Option<BilibiliDownloadOptions>,
    pub(crate) playback_options: Option<BilibiliPlaybackOptions>,
    pub(crate) output: TaskOutputRecord,
}

#[derive(Serialize, Deserialize)]
struct PersistedTaskSnapshot {
    schema_version: u32,
    #[serde(default, deserialize_with = "deserialize_persisted_tasks")]
    tasks: Vec<PersistedTaskFile>,
}

impl PersistedTaskSnapshot {
    fn validate_collection_limits(&self) -> io::Result<()> {
        validate_collection_len("persisted tasks", self.tasks.len(), MAX_PERSISTED_TASKS)?;
        for task in &self.tasks {
            validate_collection_len(
                "Bilibili task result items",
                task.result_items.len(),
                MAX_TASK_RESULTS,
            )?;
            if let Some(selection) = task.bilibili_selection.as_ref() {
                validate_collection_len(
                    "Bilibili selection ids",
                    selection.selection_ids.len(),
                    MAX_TASK_RESULTS,
                )?;
            }
            if let Some(options) = task.bilibili_options.as_ref() {
                validate_collection_len(
                    "Bilibili danmaku formats",
                    options.danmaku_formats.len(),
                    MAX_PERSISTED_DANMAKU_FORMATS,
                )?;
            }
            if let Some(session) = task.playback_session.as_ref() {
                validate_collection_len(
                    "Bilibili playback variants",
                    session.variants.len(),
                    MAX_PERSISTED_BILIBILI_VARIANTS,
                )?;
            }
            for item in &task.result_items {
                if let Some(session) = item.playback_session.as_ref() {
                    validate_collection_len(
                        "Bilibili playback variants",
                        session.variants.len(),
                        MAX_PERSISTED_BILIBILI_VARIANTS,
                    )?;
                }
            }
            if let Some(output) = task.output.as_ref() {
                validate_collection_len(
                    "task output results",
                    output.results.len(),
                    MAX_TASK_RESULTS,
                )?;
                validate_collection_len(
                    "task output resources",
                    output.resources.len(),
                    MAX_TASK_RESOURCES,
                )?;
                let artifact_count = output.results.iter().fold(0_usize, |count, result| {
                    count.saturating_add(result.artifacts.len())
                });
                validate_collection_len(
                    "task output artifacts",
                    artifact_count,
                    MAX_TASK_ARTIFACTS,
                )?;
            }
        }
        Ok(())
    }
}

fn validate_collection_len(label: &str, len: usize, limit: usize) -> io::Result<()> {
    if len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} cannot exceed {limit} entries"),
        ));
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskFile {
    id: String,
    kind: i32,
    state: i32,
    source: String,
    title: String,
    progress: f64,
    downloaded_bytes: i64,
    total_bytes: i64,
    message: String,
    library_item_id: String,
    created_at: Option<PersistedTimestamp>,
    updated_at: Option<PersistedTimestamp>,
    finished_at: Option<PersistedTimestamp>,
    #[serde(default)]
    playback_source: Option<PersistedPlaybackSource>,
    #[serde(default)]
    playback_session: Option<PersistedBilibiliPlaybackSession>,
    #[serde(default)]
    bilibili_options: Option<PersistedBilibiliDownloadOptions>,
    #[serde(default)]
    bilibili_playback_options: Option<PersistedBilibiliPlaybackOptions>,
    #[serde(default)]
    bilibili_selection: Option<PersistedBilibiliTaskSelection>,
    #[serde(default, deserialize_with = "deserialize_bilibili_result_items")]
    result_items: Vec<PersistedBilibiliTaskResultItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<PersistedTaskOutput>,
}

impl From<PersistedTaskRecord> for PersistedTaskFile {
    fn from(record: PersistedTaskRecord) -> Self {
        let task = record.task;
        Self {
            id: task.id,
            kind: task.kind,
            state: task.state,
            source: task.source,
            title: task.title,
            progress: task.progress,
            downloaded_bytes: task.downloaded_bytes,
            total_bytes: task.total_bytes,
            message: task.message,
            library_item_id: task.library_item_id,
            created_at: task.created_at.map(PersistedTimestamp::from),
            updated_at: task.updated_at.map(PersistedTimestamp::from),
            finished_at: task.finished_at.map(PersistedTimestamp::from),
            playback_source: task.playback_source.map(PersistedPlaybackSource::from),
            playback_session: task
                .playback_session
                .map(PersistedBilibiliPlaybackSession::from),
            bilibili_selection: task
                .bilibili_selection
                .map(PersistedBilibiliTaskSelection::from),
            result_items: task
                .result_items
                .into_iter()
                .map(PersistedBilibiliTaskResultItem::from)
                .collect(),
            bilibili_options: record.options.map(PersistedBilibiliDownloadOptions::from),
            bilibili_playback_options: record
                .playback_options
                .map(PersistedBilibiliPlaybackOptions::from),
            output: Some(PersistedTaskOutput::from(record.output)),
        }
    }
}

impl PersistedTaskFile {
    fn into_record(self, schema_version: u32) -> io::Result<PersistedTaskRecord> {
        let task = Task {
            id: self.id,
            kind: self.kind,
            state: self.state,
            source: self.source,
            title: self.title,
            progress: self.progress,
            downloaded_bytes: self.downloaded_bytes,
            total_bytes: self.total_bytes,
            message: self.message,
            library_item_id: self.library_item_id,
            created_at: self.created_at.map(Timestamp::from),
            updated_at: self.updated_at.map(Timestamp::from),
            finished_at: self.finished_at.map(Timestamp::from),
            playback_source: self.playback_source.map(PlaybackSource::from),
            playback_session: self.playback_session.map(BilibiliPlaybackSession::from),
            bilibili_selection: self.bilibili_selection.map(BilibiliTaskSelection::from),
            result_items: self
                .result_items
                .into_iter()
                .map(BilibiliTaskResultItem::from)
                .collect(),
            output_summary: None,
        };
        let output = match schema_version {
            LEGACY_TASK_STATE_SCHEMA_VERSION => {
                let legacy = TaskOutputRecord::from_legacy_task(&task);
                TaskOutputRecord::restored(
                    legacy.revision,
                    legacy.snapshot_id,
                    String::new(),
                    legacy.results,
                    legacy.resources,
                    legacy.legacy_managed,
                )
                .map_err(invalid_data)?
            }
            TASK_STATE_SCHEMA_VERSION => self
                .output
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "task state schema v2 task is missing output",
                    )
                })?
                .into_output()?,
            _ => unreachable!("task state schema version was validated before conversion"),
        };

        Ok(PersistedTaskRecord {
            task,
            options: self.bilibili_options.map(BilibiliDownloadOptions::from),
            playback_options: self
                .bilibili_playback_options
                .map(BilibiliPlaybackOptions::from),
            output,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskOutput {
    revision: u64,
    snapshot_id: String,
    primary_result_id: String,
    #[serde(deserialize_with = "deserialize_task_results")]
    results: Vec<PersistedTaskResult>,
    #[serde(deserialize_with = "deserialize_task_resources")]
    resources: Vec<PersistedCacheResourceRef>,
    legacy_managed: bool,
}

impl From<TaskOutputRecord> for PersistedTaskOutput {
    fn from(output: TaskOutputRecord) -> Self {
        Self {
            revision: output.revision,
            snapshot_id: output.snapshot_id,
            primary_result_id: output.primary_result_id,
            results: output
                .results
                .into_iter()
                .map(PersistedTaskResult::from)
                .collect(),
            resources: output
                .resources
                .into_iter()
                .map(PersistedCacheResourceRef::from)
                .collect(),
            legacy_managed: output.legacy_managed,
        }
    }
}

impl PersistedTaskOutput {
    fn into_output(self) -> io::Result<TaskOutputRecord> {
        let results = self
            .results
            .into_iter()
            .map(PersistedTaskResult::into_result)
            .collect::<io::Result<Vec<_>>>()?;
        let resources = self
            .resources
            .into_iter()
            .map(PersistedCacheResourceRef::into_record)
            .collect::<io::Result<Vec<_>>>()?;
        TaskOutputRecord::restored(
            self.revision,
            self.snapshot_id,
            self.primary_result_id,
            results,
            resources,
            self.legacy_managed,
        )
        .map_err(invalid_data)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskResult {
    id: String,
    state: i32,
    title: String,
    subtitle: String,
    progress: Option<PersistedTaskResultProgress>,
    problem: Option<PersistedTaskProblem>,
    library_item_id: String,
    playback_source: Option<PersistedPlaybackSource>,
    #[serde(deserialize_with = "deserialize_task_artifacts")]
    artifacts: Vec<PersistedTaskArtifact>,
    created_at: Option<PersistedTimestamp>,
    updated_at: Option<PersistedTimestamp>,
}

impl From<TaskResult> for PersistedTaskResult {
    fn from(result: TaskResult) -> Self {
        Self {
            id: result.id,
            state: result.state,
            title: result.title,
            subtitle: result.subtitle,
            progress: result.progress.map(PersistedTaskResultProgress::from),
            problem: result.problem.map(PersistedTaskProblem::from),
            library_item_id: result.library_item_id,
            playback_source: result.playback_source.map(PersistedPlaybackSource::from),
            artifacts: result
                .artifacts
                .into_iter()
                .map(PersistedTaskArtifact::from)
                .collect(),
            created_at: result.created_at.map(PersistedTimestamp::from),
            updated_at: result.updated_at.map(PersistedTimestamp::from),
        }
    }
}

impl PersistedTaskResult {
    fn into_result(self) -> io::Result<TaskResult> {
        Ok(TaskResult {
            id: self.id,
            state: self.state,
            title: self.title,
            subtitle: self.subtitle,
            progress: self.progress.map(TaskResultProgress::from),
            problem: self.problem.map(TaskProblem::from),
            library_item_id: self.library_item_id,
            playback_source: self.playback_source.map(PlaybackSource::from),
            artifacts: self
                .artifacts
                .into_iter()
                .map(PersistedTaskArtifact::into_artifact)
                .collect::<io::Result<Vec<_>>>()?,
            created_at: self.created_at.map(Timestamp::from),
            updated_at: self.updated_at.map(Timestamp::from),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskResultProgress {
    fraction: f64,
    completed_bytes: i64,
    total_bytes: i64,
    total_bytes_known: bool,
    phase: String,
    message: String,
}

impl From<TaskResultProgress> for PersistedTaskResultProgress {
    fn from(progress: TaskResultProgress) -> Self {
        Self {
            fraction: progress.fraction,
            completed_bytes: progress.completed_bytes,
            total_bytes: progress.total_bytes,
            total_bytes_known: progress.total_bytes_known,
            phase: progress.phase,
            message: progress.message,
        }
    }
}

impl From<PersistedTaskResultProgress> for TaskResultProgress {
    fn from(progress: PersistedTaskResultProgress) -> Self {
        Self {
            fraction: progress.fraction,
            completed_bytes: progress.completed_bytes,
            total_bytes: progress.total_bytes,
            total_bytes_known: progress.total_bytes_known,
            phase: progress.phase,
            message: progress.message,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskProblem {
    category: i32,
    code: String,
    message: String,
    retryable: bool,
}

impl From<TaskProblem> for PersistedTaskProblem {
    fn from(problem: TaskProblem) -> Self {
        Self {
            category: problem.category,
            code: problem.code,
            message: problem.message,
            retryable: problem.retryable,
        }
    }
}

impl From<PersistedTaskProblem> for TaskProblem {
    fn from(problem: PersistedTaskProblem) -> Self {
        Self {
            category: problem.category,
            code: problem.code,
            message: problem.message,
            retryable: problem.retryable,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskArtifact {
    id: String,
    kind: i32,
    state: i32,
    title: String,
    format: String,
    language_tag: String,
    is_ai_generated: bool,
    resource: Option<PersistedCacheResourceRef>,
    problem: Option<PersistedTaskProblem>,
}

impl From<TaskArtifact> for PersistedTaskArtifact {
    fn from(artifact: TaskArtifact) -> Self {
        Self {
            id: artifact.id,
            kind: artifact.kind,
            state: artifact.state,
            title: artifact.title,
            format: artifact.format,
            language_tag: artifact.language_tag,
            is_ai_generated: artifact.is_ai_generated,
            resource: artifact.resource.map(PersistedCacheResourceRef::from),
            problem: artifact.problem.map(PersistedTaskProblem::from),
        }
    }
}

impl PersistedTaskArtifact {
    fn into_artifact(self) -> io::Result<TaskArtifact> {
        Ok(TaskArtifact {
            id: self.id,
            kind: self.kind,
            state: self.state,
            title: self.title,
            format: self.format,
            language_tag: self.language_tag,
            is_ai_generated: self.is_ai_generated,
            resource: self
                .resource
                .map(PersistedCacheResourceRef::into_record)
                .transpose()?
                .map(|record| record.resource),
            problem: self.problem.map(TaskProblem::from),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedCacheResourceRef {
    id: String,
    content_type: String,
    size_bytes: i64,
    size_known: bool,
    supports_byte_ranges: bool,
    etag: String,
    expires_at: Option<PersistedTimestamp>,
}

impl From<TaskResourceRecord> for PersistedCacheResourceRef {
    fn from(record: TaskResourceRecord) -> Self {
        Self::from(record.resource)
    }
}

impl From<CacheResourceRef> for PersistedCacheResourceRef {
    fn from(resource: CacheResourceRef) -> Self {
        Self {
            id: resource.id,
            content_type: resource.content_type,
            size_bytes: resource.size_bytes,
            size_known: resource.size_known,
            supports_byte_ranges: resource.supports_byte_ranges,
            etag: resource.etag,
            expires_at: resource.expires_at.map(PersistedTimestamp::from),
        }
    }
}

impl PersistedCacheResourceRef {
    fn into_record(self) -> io::Result<TaskResourceRecord> {
        TaskResourceRecord::new(CacheResourceRef {
            id: self.id,
            uri: String::new(),
            content_type: self.content_type,
            size_bytes: self.size_bytes,
            size_known: self.size_known,
            supports_byte_ranges: self.supports_byte_ranges,
            etag: self.etag,
            expires_at: self.expires_at.map(Timestamp::from),
        })
        .map_err(invalid_data)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliTaskSelection {
    mode: i32,
    #[serde(default, deserialize_with = "deserialize_selection_ids")]
    selection_ids: Vec<String>,
    range_start_index: u32,
    range_end_index: u32,
}

impl From<BilibiliTaskSelection> for PersistedBilibiliTaskSelection {
    fn from(selection: BilibiliTaskSelection) -> Self {
        Self {
            mode: selection.mode,
            selection_ids: selection.selection_ids,
            range_start_index: selection.range_start_index,
            range_end_index: selection.range_end_index,
        }
    }
}

impl From<PersistedBilibiliTaskSelection> for BilibiliTaskSelection {
    fn from(selection: PersistedBilibiliTaskSelection) -> Self {
        Self {
            mode: selection.mode,
            selection_ids: selection.selection_ids,
            range_start_index: selection.range_start_index,
            range_end_index: selection.range_end_index,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliTaskResultItem {
    id: String,
    selection_id: String,
    title: String,
    subtitle: String,
    source_kind: String,
    content_id: String,
    index: u32,
    state: i32,
    message: String,
    library_item_id: String,
    playback_source: Option<PersistedPlaybackSource>,
    playback_session: Option<PersistedBilibiliPlaybackSession>,
}

impl From<BilibiliTaskResultItem> for PersistedBilibiliTaskResultItem {
    fn from(item: BilibiliTaskResultItem) -> Self {
        Self {
            id: item.id,
            selection_id: item.selection_id,
            title: item.title,
            subtitle: item.subtitle,
            source_kind: item.source_kind,
            content_id: item.content_id,
            index: item.index,
            state: item.state,
            message: item.message,
            library_item_id: item.library_item_id,
            playback_source: item.playback_source.map(PersistedPlaybackSource::from),
            playback_session: item
                .playback_session
                .map(PersistedBilibiliPlaybackSession::from),
        }
    }
}

impl From<PersistedBilibiliTaskResultItem> for BilibiliTaskResultItem {
    fn from(item: PersistedBilibiliTaskResultItem) -> Self {
        Self {
            id: item.id,
            selection_id: item.selection_id,
            title: item.title,
            subtitle: item.subtitle,
            source_kind: item.source_kind,
            content_id: item.content_id,
            index: item.index,
            state: item.state,
            message: item.message,
            library_item_id: item.library_item_id,
            playback_source: item.playback_source.map(PlaybackSource::from),
            playback_session: item.playback_session.map(BilibiliPlaybackSession::from),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTimestamp {
    seconds: i64,
    nanos: i32,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedPlaybackSource {
    item_id: String,
    variant_id: String,
    protocol: i32,
    uri: String,
    expires_at: Option<PersistedTimestamp>,
}

impl From<PlaybackSource> for PersistedPlaybackSource {
    fn from(source: PlaybackSource) -> Self {
        Self {
            item_id: source.item_id,
            variant_id: source.variant_id,
            protocol: source.protocol,
            uri: source.uri,
            expires_at: source.expires_at.map(PersistedTimestamp::from),
        }
    }
}

impl From<PersistedPlaybackSource> for PlaybackSource {
    fn from(source: PersistedPlaybackSource) -> Self {
        Self {
            item_id: source.item_id,
            variant_id: source.variant_id,
            protocol: source.protocol,
            uri: source.uri,
            expires_at: source.expires_at.map(Timestamp::from),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliPlaybackSession {
    id: String,
    title: String,
    content_id: String,
    selected_variant_id: String,
    selected_variant: Option<PersistedBilibiliPlaybackVariant>,
    #[serde(default, deserialize_with = "deserialize_playback_variants")]
    variants: Vec<PersistedBilibiliPlaybackVariant>,
    #[serde(default)]
    transcoding_plan: Option<PersistedLanTranscodingPlan>,
    #[serde(default)]
    effective_policy: Option<PlaybackPolicy>,
}

impl From<BilibiliPlaybackSession> for PersistedBilibiliPlaybackSession {
    fn from(session: BilibiliPlaybackSession) -> Self {
        Self {
            id: session.id,
            title: session.title,
            content_id: session.content_id,
            selected_variant_id: session.selected_variant_id,
            selected_variant: session
                .selected_variant
                .map(PersistedBilibiliPlaybackVariant::from),
            variants: session
                .variants
                .into_iter()
                .map(PersistedBilibiliPlaybackVariant::from)
                .collect(),
            transcoding_plan: session
                .transcoding_plan
                .map(PersistedLanTranscodingPlan::from),
            effective_policy: session.effective_policy.map(|policy| {
                PlaybackPolicy::from_proto(Some(&policy))
                    .expect("server-owned playback session policy should use known enum values")
            }),
        }
    }
}

impl From<PersistedBilibiliPlaybackSession> for BilibiliPlaybackSession {
    fn from(session: PersistedBilibiliPlaybackSession) -> Self {
        Self {
            id: session.id,
            title: session.title,
            content_id: session.content_id,
            selected_variant_id: session.selected_variant_id,
            selected_variant: session.selected_variant.map(BilibiliPlaybackVariant::from),
            variants: session
                .variants
                .into_iter()
                .map(BilibiliPlaybackVariant::from)
                .collect(),
            transcoding_plan: session.transcoding_plan.map(LanTranscodingPlan::from),
            effective_policy: Some(session.effective_policy.unwrap_or_default().to_proto()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedLanTranscodingPlan {
    state: i32,
    profile_id: String,
    reason: String,
    source_variant_id: String,
    target_container: String,
    target_video_codec: String,
    target_audio_codec: String,
    output_protocol: i32,
}

impl From<LanTranscodingPlan> for PersistedLanTranscodingPlan {
    fn from(plan: LanTranscodingPlan) -> Self {
        Self {
            state: plan.state,
            profile_id: plan.profile_id,
            reason: plan.reason,
            source_variant_id: plan.source_variant_id,
            target_container: plan.target_container,
            target_video_codec: plan.target_video_codec,
            target_audio_codec: plan.target_audio_codec,
            output_protocol: plan.output_protocol,
        }
    }
}

impl From<PersistedLanTranscodingPlan> for LanTranscodingPlan {
    fn from(plan: PersistedLanTranscodingPlan) -> Self {
        Self {
            state: plan.state,
            profile_id: plan.profile_id,
            reason: plan.reason,
            source_variant_id: plan.source_variant_id,
            target_container: plan.target_container,
            target_video_codec: plan.target_video_codec,
            target_audio_codec: plan.target_audio_codec,
            output_protocol: plan.output_protocol,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliPlaybackVariant {
    id: String,
    label: String,
    source_kind: String,
    container: String,
    video_codec: String,
    audio_codec: String,
    width: i32,
    height: i32,
    bitrate: i64,
    size_bytes: i64,
}

impl From<BilibiliPlaybackVariant> for PersistedBilibiliPlaybackVariant {
    fn from(variant: BilibiliPlaybackVariant) -> Self {
        Self {
            id: variant.id,
            label: variant.label,
            source_kind: variant.source_kind,
            container: variant.container,
            video_codec: variant.video_codec,
            audio_codec: variant.audio_codec,
            width: variant.width,
            height: variant.height,
            bitrate: variant.bitrate,
            size_bytes: variant.size_bytes,
        }
    }
}

impl From<PersistedBilibiliPlaybackVariant> for BilibiliPlaybackVariant {
    fn from(variant: PersistedBilibiliPlaybackVariant) -> Self {
        Self {
            id: variant.id,
            label: variant.label,
            source_kind: variant.source_kind,
            container: variant.container,
            video_codec: variant.video_codec,
            audio_codec: variant.audio_codec,
            width: variant.width,
            height: variant.height,
            bitrate: variant.bitrate,
            size_bytes: variant.size_bytes,
        }
    }
}

impl From<Timestamp> for PersistedTimestamp {
    fn from(timestamp: Timestamp) -> Self {
        Self {
            seconds: timestamp.seconds,
            nanos: timestamp.nanos,
        }
    }
}

impl From<PersistedTimestamp> for Timestamp {
    fn from(timestamp: PersistedTimestamp) -> Self {
        Self {
            seconds: timestamp.seconds,
            nanos: timestamp.nanos,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliDownloadOptions {
    quality_preference: String,
    encoding_preference: String,
    prefer_tv_api: bool,
    download_subtitles: bool,
    download_danmaku: bool,
    #[serde(default)]
    audio_language: String,
    #[serde(default)]
    subtitle_ai_policy: i32,
    #[serde(default)]
    download_cover: bool,
    #[serde(default, deserialize_with = "deserialize_danmaku_formats")]
    danmaku_formats: Vec<i32>,
}

impl From<BilibiliDownloadOptions> for PersistedBilibiliDownloadOptions {
    fn from(options: BilibiliDownloadOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            download_subtitles: options.download_subtitles,
            download_danmaku: options.download_danmaku,
            audio_language: options.audio_language,
            subtitle_ai_policy: options.subtitle_ai_policy,
            download_cover: options.download_cover,
            danmaku_formats: options.danmaku_formats,
        }
    }
}

impl From<PersistedBilibiliDownloadOptions> for BilibiliDownloadOptions {
    fn from(options: PersistedBilibiliDownloadOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            download_subtitles: options.download_subtitles,
            download_danmaku: options.download_danmaku,
            audio_language: options.audio_language,
            subtitle_ai_policy: options.subtitle_ai_policy,
            download_cover: options.download_cover,
            danmaku_formats: options.danmaku_formats,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliPlaybackOptions {
    quality_preference: String,
    encoding_preference: String,
    prefer_tv_api: bool,
    #[serde(default)]
    audio_language: String,
    #[serde(default)]
    playback_policy: Option<PlaybackPolicy>,
}

impl From<BilibiliPlaybackOptions> for PersistedBilibiliPlaybackOptions {
    fn from(options: BilibiliPlaybackOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            audio_language: options.audio_language,
            playback_policy: options.playback_policy.map(|policy| {
                PlaybackPolicy::from_proto(Some(&policy))
                    .expect("validated playback options should use known policy enum values")
            }),
        }
    }
}

impl From<PersistedBilibiliPlaybackOptions> for BilibiliPlaybackOptions {
    fn from(options: PersistedBilibiliPlaybackOptions) -> Self {
        Self {
            quality_preference: options.quality_preference,
            encoding_preference: options.encoding_preference,
            prefer_tv_api: options.prefer_tv_api,
            audio_language: options.audio_language,
            playback_policy: options.playback_policy.map(PlaybackPolicy::to_proto),
        }
    }
}

fn temp_path_for(path: &Path) -> PathBuf {
    let temp_name = path
        .file_name()
        .map(|name| format!("{}.tmp", name.to_string_lossy()))
        .unwrap_or_else(|| "tasks.json.tmp".to_owned());
    path.with_file_name(temp_name)
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tvos_net_player::v1::{
        BilibiliCompatibleVariantPreference, BilibiliDanmakuFormat, BilibiliPlaybackPolicy,
        BilibiliPlaybackVariant, BilibiliSubtitleAiPolicy, BilibiliTaskResultItem,
        BilibiliTaskSelection, BilibiliTranscodingPreference, BilibiliWeakNetworkPreference,
        LanTranscodingPlan, LanTranscodingPlanState, PlaybackProtocol, TaskArtifactKind,
        TaskArtifactState, TaskKind, TaskProblemCategory, TaskState,
    };

    fn persisted_task_artifact_fixture(id: &str) -> PersistedTaskArtifact {
        PersistedTaskArtifact {
            id: id.to_owned(),
            kind: TaskArtifactKind::Metadata.into(),
            state: TaskArtifactState::Available.into(),
            title: String::new(),
            format: String::new(),
            language_tag: String::new(),
            is_ai_generated: false,
            resource: None,
            problem: None,
        }
    }

    fn persisted_task_result_fixture(
        id: &str,
        artifacts: Vec<PersistedTaskArtifact>,
    ) -> PersistedTaskResult {
        PersistedTaskResult {
            id: id.to_owned(),
            state: TaskState::Completed.into(),
            title: String::new(),
            subtitle: String::new(),
            progress: None,
            problem: None,
            library_item_id: String::new(),
            playback_source: None,
            artifacts,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn bounded_snapshot_writer_counts_the_trailing_newline() {
        let mut writer = BoundedSnapshotWriter::new(4);
        writer.write_all(b"abc").unwrap();
        writer.write_all(b"\n").unwrap();
        assert_eq!(b"abc\n", writer.into_inner().as_slice());

        let mut writer = BoundedSnapshotWriter::new(3);
        writer.write_all(b"abc").unwrap();
        let error = writer
            .write_all(b"\n")
            .expect_err("the trailing newline must stay inside the snapshot limit");
        assert_eq!(io::ErrorKind::InvalidData, error.kind());
    }

    #[test]
    fn save_rejects_collections_that_the_loader_would_reject() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let task = Task {
            id: "task-one".to_owned(),
            source: "BV1collection-limit".to_owned(),
            ..Default::default()
        };
        let record = PersistedTaskRecord {
            output: TaskOutputRecord::from_legacy_task(&task),
            task,
            options: Some(BilibiliDownloadOptions {
                danmaku_formats: vec![0; MAX_PERSISTED_DANMAKU_FORMATS + 1],
                ..Default::default()
            }),
            playback_options: None,
        };

        let error = TaskStateStore::new(&path)
            .save(&[record])
            .expect_err("writer and loader collection limits must match");

        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert!(!path.exists());
    }

    #[test]
    fn persisted_resource_total_stays_below_the_startup_scan_headroom() {
        let task = Task {
            id: "task-resource-limit".to_owned(),
            source: "BV1resource-limit".to_owned(),
            ..Default::default()
        };
        let mut output = TaskOutputRecord::from_legacy_task(&task);
        output.resources = (0..MAX_REGISTERED_TASK_RESOURCES)
            .map(|index| {
                TaskResourceRecord::new(CacheResourceRef {
                    id: format!("resource-{index}"),
                    ..Default::default()
                })
                .unwrap()
            })
            .collect();
        let mut record = PersistedTaskRecord {
            output,
            task,
            options: None,
            playback_options: None,
        };

        validate_registered_task_resource_count(std::slice::from_ref(&record))
            .expect("the registered resource limit should be accepted");
        record.output.resources.push(
            TaskResourceRecord::new(CacheResourceRef {
                id: "resource-over-limit".to_owned(),
                ..Default::default()
            })
            .unwrap(),
        );

        let error = validate_registered_task_resource_count(std::slice::from_ref(&record))
            .expect_err("one additional registered resource must be rejected");
        assert_eq!(io::ErrorKind::InvalidData, error.kind());
    }

    #[test]
    fn persisted_resource_budget_is_enforced_while_snapshot_resources_are_decoded() {
        let persisted_task = |task_id: &str, resource_ids: &[&str]| {
            let task = Task {
                id: task_id.to_owned(),
                source: format!("BV1{task_id}"),
                ..Default::default()
            };
            let mut output = TaskOutputRecord::from_legacy_task(&task);
            output.resources = resource_ids
                .iter()
                .map(|resource_id| {
                    TaskResourceRecord::new(CacheResourceRef {
                        id: (*resource_id).to_owned(),
                        ..Default::default()
                    })
                    .unwrap()
                })
                .collect();
            PersistedTaskFile::from(PersistedTaskRecord {
                task,
                options: None,
                playback_options: None,
                output,
            })
        };
        let accepted = PersistedTaskSnapshot {
            schema_version: TASK_STATE_SCHEMA_VERSION,
            tasks: vec![
                persisted_task("budget-one", &["resource-one"]),
                persisted_task("budget-two", &["resource-two"]),
            ],
        };
        let accepted_bytes = serde_json::to_vec(&accepted).unwrap();

        let decoded = deserialize_task_snapshot_with_resource_limit(&accepted_bytes, 2)
            .expect("resources at the snapshot limit should decode");
        assert_eq!(2, decoded.tasks.len());

        let rejected = PersistedTaskSnapshot {
            schema_version: TASK_STATE_SCHEMA_VERSION,
            tasks: vec![
                persisted_task("budget-three", &["resource-three", "resource-four"]),
                persisted_task("budget-four", &["resource-five"]),
            ],
        };
        let rejected_bytes = serde_json::to_vec(&rejected).unwrap();
        let error = match deserialize_task_snapshot_with_resource_limit(&rejected_bytes, 2) {
            Ok(_) => panic!("the third resource must be rejected during deserialization"),
            Err(error) => error,
        };

        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert!(
            error
                .to_string()
                .contains("cannot contain more than 2 registered resources")
        );
    }

    #[test]
    fn bounded_vector_deserializer_rejects_an_extra_item_without_retaining_it() {
        let mut deserializer = serde_json::Deserializer::from_str("[1, 2, 3]");
        let parsed: Result<Vec<u8>, _> =
            deserialize_bounded_vec(&mut deserializer, 2, "test values");

        let error = parsed.expect_err("the third item must exceed the bound");
        assert!(error.to_string().contains("cannot exceed 2"));
    }

    #[test]
    fn persisted_task_output_bounds_results_during_deserialization() {
        let result = PersistedTaskResult {
            id: "result".to_owned(),
            state: TaskState::Completed.into(),
            title: String::new(),
            subtitle: String::new(),
            progress: None,
            problem: None,
            library_item_id: String::new(),
            playback_source: None,
            artifacts: Vec::new(),
            created_at: None,
            updated_at: None,
        };
        let output = PersistedTaskOutput {
            revision: 1,
            snapshot_id: "snapshot".to_owned(),
            primary_result_id: String::new(),
            results: vec![result; MAX_TASK_RESULTS + 1],
            resources: Vec::new(),
            legacy_managed: false,
        };
        let bytes = serde_json::to_vec(&output).expect("fixture should serialize");

        let error = match serde_json::from_slice::<PersistedTaskOutput>(&bytes) {
            Ok(_) => panic!("persisted results must be bounded while decoding"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("task output results cannot exceed")
        );
    }

    #[test]
    fn persisted_task_output_rejects_cross_result_artifact_before_decoding_it() {
        let artifact = persisted_task_artifact_fixture("artifact");
        let results = vec![
            persisted_task_result_fixture("first", vec![artifact.clone(), artifact.clone()]),
            persisted_task_result_fixture("second", vec![artifact]),
        ];
        let mut encoded_results = serde_json::to_value(results).expect("fixture should serialize");
        // A typed decode would reject this before the former post-result aggregate check.
        encoded_results[1]["artifacts"][0]["kind"] =
            serde_json::Value::String("must-not-decode".to_owned());
        let bytes = serde_json::to_vec(&encoded_results).expect("fixture should serialize");
        let mut deserializer = serde_json::Deserializer::from_slice(&bytes);

        let error = match deserialize_task_results_with_artifact_limit(&mut deserializer, 2) {
            Ok(_) => panic!("the third artifact must be rejected before typed decoding"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("task output artifacts cannot exceed 2 entries"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn persisted_task_output_accepts_cross_result_artifacts_at_aggregate_limit() {
        let results = vec![
            persisted_task_result_fixture(
                "first",
                vec![persisted_task_artifact_fixture("artifact-one")],
            ),
            persisted_task_result_fixture(
                "second",
                vec![persisted_task_artifact_fixture("artifact-two")],
            ),
        ];
        let bytes = serde_json::to_vec(&results).expect("fixture should serialize");
        let mut deserializer = serde_json::Deserializer::from_slice(&bytes);

        let decoded = deserialize_task_results_with_artifact_limit(&mut deserializer, 2)
            .expect("artifacts at the aggregate limit should decode");
        deserializer
            .end()
            .expect("fixture should be fully consumed");

        assert_eq!(2, decoded.len());
        assert_eq!(1, decoded[0].artifacts.len());
        assert_eq!(1, decoded[1].artifacts.len());
    }

    #[test]
    fn save_creates_initially_missing_nested_state_directory() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_directory = temp.path().join("state").join("task-store");
        let path = state_directory.join("tasks.json");
        assert!(!state_directory.exists());

        let store = TaskStateStore::new(path.clone());
        store
            .save(&[])
            .expect("task state should persist in a new nested directory");

        assert!(path.is_file());
        assert!(
            store
                .load()
                .expect("persisted task state should load")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepares_new_parent_directories_for_bottom_up_sync() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let first_directory = temp.path().join("state");
        let state_directory = first_directory.join("task-store");
        let path = state_directory.join("tasks.json");

        let directories_to_sync =
            prepare_parent_directory(&path).expect("nested parent directories should be prepared");
        let expected_directories = vec![
            state_directory.clone(),
            first_directory.clone(),
            temp.path().to_path_buf(),
        ];

        assert_eq!(expected_directories, directories_to_sync);
        assert_eq!(state_directory, directories_to_sync[0]);
        assert_eq!(first_directory, directories_to_sync[1]);
        assert_eq!(temp.path(), directories_to_sync[2]);
        assert!(state_directory.is_dir());
        sync_directories(&directories_to_sync).expect("prepared directories should be syncable");

        assert_eq!(
            vec![state_directory],
            prepare_parent_directory(&path)
                .expect("an existing parent directory should still be prepared")
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_sync_retry_retains_the_original_creation_chain() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let first_directory = temp.path().join("state");
        let state_directory = first_directory.join("task-store");
        let path = state_directory.join("tasks.json");
        let expected_directories = path
            .parent()
            .expect("state file should have a parent")
            .ancestors()
            .map(|ancestor| {
                if ancestor.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    ancestor.to_path_buf()
                }
            })
            .collect::<Vec<_>>();
        let store = TaskStateStore::new(path.clone());

        store.fail_next_directory_sync();
        assert!(matches!(
            store.save(&[]).expect("rename should commit the snapshot"),
            TaskStateSaveOutcome::InstalledButNotDurable(_)
        ));
        assert_eq!(expected_directories, store.pending_directory_syncs());

        assert!(matches!(
            store
                .save(&[])
                .expect("directory sync retry should succeed"),
            TaskStateSaveOutcome::Durable
        ));
        assert!(store.pending_directory_syncs().is_empty());

        let restarted_store = TaskStateStore::new(path);
        restarted_store.fail_next_directory_sync();
        assert!(matches!(
            restarted_store
                .save(&[])
                .expect("restart retry should install the snapshot"),
            TaskStateSaveOutcome::InstalledButNotDurable(_)
        ));
        assert_eq!(
            expected_directories,
            restarted_store.pending_directory_syncs(),
            "a restarted store must reconstruct the complete ancestor sync chain"
        );
    }

    #[test]
    fn load_legacy_snapshot_defaults_bilibili_schema_fields() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "tasks": [
    {
      "id": "bilibili-playback-legacy",
      "kind": 3,
      "state": 10,
      "source": "BV1legacy",
      "title": "Legacy video",
      "progress": 1.0,
      "downloaded_bytes": 0,
      "total_bytes": 0,
      "message": "Cached.",
      "library_item_id": "bilibili.hls.legacy",
      "created_at": null,
      "updated_at": null,
      "finished_at": null
    }
  ]
}"#,
        )
        .expect("legacy snapshot should be written");

        let records = TaskStateStore::new(path)
            .load()
            .expect("legacy snapshot should load");

        assert_eq!(1, records.len());
        assert!(records[0].task.bilibili_selection.is_none());
        assert!(records[0].task.result_items.is_empty());
    }

    #[test]
    fn migrates_v1_snapshot_to_legacy_managed_output() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "tasks": [
    {
      "id": "bilibili-v1-output",
      "kind": 1,
      "state": 10,
      "source": "BV1legacyOutput",
      "title": "Legacy collection",
      "progress": 1.0,
      "downloaded_bytes": 200,
      "total_bytes": 200,
      "message": "Finished.",
      "library_item_id": "",
      "created_at": { "seconds": 100, "nanos": 1 },
      "updated_at": { "seconds": 200, "nanos": 2 },
      "finished_at": { "seconds": 200, "nanos": 2 },
      "result_items": [
        {
          "id": "legacy-result-second",
          "selection_id": "page:2",
          "title": "Second",
          "subtitle": "Page 2",
          "source_kind": "video_page",
          "content_id": "cid-2",
          "index": 2,
          "state": 4,
          "message": "Provider failed.",
          "library_item_id": ""
        },
        {
          "id": "legacy-result-first",
          "selection_id": "page:1",
          "title": "First",
          "subtitle": "Page 1",
          "source_kind": "video_page",
          "content_id": "cid-1",
          "index": 1,
          "state": 10,
          "message": "Cached.",
          "library_item_id": "bilibili.hls.first"
        }
      ]
    }
  ]
}"#,
        )
        .expect("v1 snapshot should be written");

        let records = TaskStateStore::new(path)
            .load()
            .expect("v1 snapshot should migrate");
        let output = &records[0].output;

        assert_eq!(1, output.revision);
        assert!(output.snapshot_id.starts_with("task-output-"));
        assert_eq!("legacy-result-first", output.primary_result_id);
        assert!(output.legacy_managed);
        assert!(output.resources.is_empty());
        assert_eq!(
            vec!["legacy-result-second", "legacy-result-first"],
            output
                .results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(TaskState::Failed, output.results[0].state());
        assert_eq!("Page 2", output.results[0].subtitle);
        assert_eq!(
            Some(TaskProblemCategory::Upstream),
            output.results[0]
                .problem
                .as_ref()
                .map(|problem| problem.category())
        );
        assert_eq!(
            1.0,
            output.results[1]
                .progress
                .as_ref()
                .expect("legacy result progress should exist")
                .fraction
        );
        assert_eq!(
            Some(Timestamp {
                seconds: 100,
                nanos: 1,
            }),
            output.results[0].created_at
        );
    }

    #[test]
    fn load_legacy_playback_sessions_default_effective_policy() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "tasks": [
    {
      "id": "bilibili-playback-legacy-policy",
      "kind": 3,
      "state": 6,
      "source": "BV1legacy",
      "title": "Legacy playback",
      "progress": 0.5,
      "downloaded_bytes": 0,
      "total_bytes": 0,
      "message": "Playable.",
      "library_item_id": "",
      "created_at": null,
      "updated_at": null,
      "finished_at": null,
      "playback_source": null,
      "playback_session": {
        "id": "bilibili-playback-legacy-policy",
        "title": "Legacy playback",
        "content_id": "cid-primary",
        "selected_variant_id": "h264",
        "selected_variant": null
      },
      "result_items": [
        {
          "id": "bilibili-playback-legacy-policy-result-1",
          "selection_id": "page:1",
          "title": "Part 1",
          "subtitle": "Page 1",
          "source_kind": "video_page",
          "content_id": "cid-result",
          "index": 1,
          "state": 6,
          "message": "Playable.",
          "library_item_id": "",
          "playback_source": null,
          "playback_session": {
            "id": "bilibili-playback-legacy-policy-result-1",
            "title": "Part 1",
            "content_id": "cid-result",
            "selected_variant_id": "h264",
            "selected_variant": null
          }
        }
      ]
    }
  ]
}"#,
        )
        .expect("legacy playback snapshot should be written");

        let records = TaskStateStore::new(path)
            .load()
            .expect("legacy playback snapshot should load");
        let expected = PlaybackPolicy::default().to_proto();

        assert_eq!(
            Some(&expected),
            records[0]
                .task
                .playback_session
                .as_ref()
                .and_then(|session| session.effective_policy.as_ref())
        );
        assert_eq!(
            Some(&expected),
            records[0].task.result_items[0]
                .playback_session
                .as_ref()
                .and_then(|session| session.effective_policy.as_ref())
        );
    }

    #[test]
    fn load_legacy_bilibili_options_defaults_new_schema_fields() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "tasks": [
    {
      "id": "bilibili-options-legacy",
      "kind": 1,
      "state": 1,
      "source": "BV1legacy",
      "title": "Legacy options",
      "progress": 0.0,
      "downloaded_bytes": 0,
      "total_bytes": 0,
      "message": "Queued.",
      "library_item_id": "",
      "created_at": null,
      "updated_at": null,
      "finished_at": null,
      "bilibili_options": {
        "quality_preference": "1080p",
        "encoding_preference": "",
        "prefer_tv_api": true,
        "download_subtitles": true,
        "download_danmaku": false
      },
      "bilibili_playback_options": {
        "quality_preference": "720p",
        "encoding_preference": "h264",
        "prefer_tv_api": false
      }
    }
  ]
}"#,
        )
        .expect("legacy options snapshot should be written");

        let records = TaskStateStore::new(path)
            .load()
            .expect("legacy options snapshot should load");

        let options = records[0]
            .options
            .as_ref()
            .expect("download options should restore");
        assert_eq!("1080p", options.quality_preference);
        assert!(options.prefer_tv_api);
        assert!(options.download_subtitles);
        assert!(options.audio_language.is_empty());
        assert_eq!(
            BilibiliSubtitleAiPolicy::Unspecified,
            options.subtitle_ai_policy()
        );
        assert!(!options.download_cover);
        assert!(options.danmaku_formats.is_empty());

        let playback_options = records[0]
            .playback_options
            .as_ref()
            .expect("playback options should restore");
        assert_eq!("720p", playback_options.quality_preference);
        assert!(playback_options.audio_language.is_empty());
        assert!(playback_options.playback_policy.is_none());
    }

    #[test]
    fn round_trips_nested_v2_task_output_without_persisting_resource_locations() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let subtitle_resource = TaskResourceRecord::new(CacheResourceRef {
            id: "subtitle-z".to_owned(),
            uri: "file:///private/tmp/must-not-persist".to_owned(),
            content_type: "text/vtt; charset=utf-8".to_owned(),
            size_bytes: 321,
            size_known: true,
            supports_byte_ranges: true,
            etag: "subtitle-etag".to_owned(),
            expires_at: Some(Timestamp {
                seconds: 10_000,
                nanos: 11,
            }),
        })
        .expect("subtitle resource should be valid");
        let cover_resource = TaskResourceRecord::new(CacheResourceRef {
            id: "cover-a".to_owned(),
            uri: "https://upstream.example.test/private-cover".to_owned(),
            content_type: "image/jpeg".to_owned(),
            size_bytes: 654,
            size_known: false,
            supports_byte_ranges: false,
            etag: "cover-etag".to_owned(),
            expires_at: Some(Timestamp {
                seconds: 20_000,
                nanos: 22,
            }),
        })
        .expect("cover resource should be valid");
        let results = vec![
            TaskResult {
                id: "result-z".to_owned(),
                state: TaskState::Failed.into(),
                title: "Failed result".to_owned(),
                subtitle: "Nested fields".to_owned(),
                progress: Some(TaskResultProgress {
                    fraction: 0.75,
                    completed_bytes: 750,
                    total_bytes: 1_000,
                    total_bytes_known: true,
                    phase: "packaging".to_owned(),
                    message: "Packaging artifacts.".to_owned(),
                }),
                problem: Some(TaskProblem {
                    category: TaskProblemCategory::Upstream.into(),
                    code: "bilibili.test_failure".to_owned(),
                    message: "Provider failed.".to_owned(),
                    retryable: true,
                }),
                library_item_id: "library.result-z".to_owned(),
                playback_source: Some(PlaybackSource {
                    item_id: "library.result-z".to_owned(),
                    variant_id: "h264".to_owned(),
                    protocol: PlaybackProtocol::Hls.into(),
                    uri: "http://media.example.test/result-z/master.m3u8".to_owned(),
                    expires_at: Some(Timestamp {
                        seconds: 30_000,
                        nanos: 33,
                    }),
                }),
                artifacts: vec![
                    TaskArtifact {
                        id: "artifact-cover".to_owned(),
                        kind: TaskArtifactKind::CoverImage.into(),
                        state: TaskArtifactState::Unavailable.into(),
                        title: "Cover".to_owned(),
                        format: "jpeg".to_owned(),
                        language_tag: String::new(),
                        is_ai_generated: false,
                        resource: Some(cover_resource.resource.clone()),
                        problem: Some(TaskProblem {
                            category: TaskProblemCategory::Permission.into(),
                            code: "artifact.cover_denied".to_owned(),
                            message: "Cover access denied.".to_owned(),
                            retryable: false,
                        }),
                    },
                    TaskArtifact {
                        id: "artifact-subtitle".to_owned(),
                        kind: TaskArtifactKind::Subtitle.into(),
                        state: TaskArtifactState::Available.into(),
                        title: "Japanese subtitles".to_owned(),
                        format: "vtt".to_owned(),
                        language_tag: "ja-JP".to_owned(),
                        is_ai_generated: true,
                        resource: Some(subtitle_resource.resource.clone()),
                        problem: None,
                    },
                ],
                created_at: Some(Timestamp {
                    seconds: 100,
                    nanos: 1,
                }),
                updated_at: Some(Timestamp {
                    seconds: 200,
                    nanos: 2,
                }),
            },
            TaskResult {
                id: "result-a".to_owned(),
                state: TaskState::Completed.into(),
                title: "Completed result".to_owned(),
                subtitle: "Second result".to_owned(),
                progress: None,
                problem: None,
                library_item_id: "library.result-a".to_owned(),
                playback_source: None,
                artifacts: Vec::new(),
                created_at: None,
                updated_at: None,
            },
        ];
        let output = TaskOutputRecord::restored(
            42,
            "snapshot-v2-nested".to_owned(),
            "result-a".to_owned(),
            results,
            vec![subtitle_resource, cover_resource],
            false,
        )
        .expect("nested output should be valid");
        let task = Task {
            id: "task-v2-output".to_owned(),
            kind: TaskKind::BilibiliDownload.into(),
            state: TaskState::Failed.into(),
            source: "BV1nestedOutput".to_owned(),
            ..Default::default()
        };
        TaskStateStore::new(path.clone())
            .save(&[PersistedTaskRecord {
                task,
                options: None,
                playback_options: None,
                output: output.clone(),
            }])
            .expect("v2 task output should persist");

        let mut snapshot: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).expect("persisted snapshot should be readable"),
        )
        .expect("persisted snapshot should be valid JSON");
        assert_eq!(Some(2), snapshot["schema_version"].as_u64());
        let resource_json = snapshot["tasks"][0]["output"]["resources"][0]
            .as_object()
            .expect("persisted output resource should be an object");
        assert!(!resource_json.contains_key("uri"));
        assert!(!resource_json.contains_key("relative_path"));
        assert!(!resource_json.contains_key("local_path"));
        let artifact_resource_json =
            snapshot["tasks"][0]["output"]["results"][0]["artifacts"][0]["resource"]
                .as_object()
                .expect("persisted artifact resource should be an object");
        assert!(!artifact_resource_json.contains_key("uri"));
        assert!(!artifact_resource_json.contains_key("relative_path"));
        assert!(!artifact_resource_json.contains_key("local_path"));

        snapshot["tasks"][0]["output"]["resources"][0]
            .as_object_mut()
            .expect("persisted output resource should be mutable")
            .insert(
                "uri".to_owned(),
                serde_json::Value::String("file:///private/tmp/tampered".to_owned()),
            );
        snapshot["tasks"][0]["output"]["results"][0]["artifacts"][0]["resource"]
            .as_object_mut()
            .expect("persisted artifact resource should be mutable")
            .insert(
                "uri".to_owned(),
                serde_json::Value::String("https://upstream.example.test/tampered".to_owned()),
            );
        fs::write(
            &path,
            serde_json::to_vec_pretty(&snapshot).expect("tampered snapshot should serialize"),
        )
        .expect("tampered snapshot should be written");

        let records = TaskStateStore::new(path.clone())
            .load()
            .expect("v2 task output should reload");
        assert_eq!(output, records[0].output);
        assert_eq!("result-a", records[0].output.primary_result_id);
        assert_eq!(
            vec!["result-z", "result-a"],
            records[0]
                .output
                .results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            vec!["artifact-cover", "artifact-subtitle"],
            records[0].output.results[0]
                .artifacts
                .iter()
                .map(|artifact| artifact.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            vec!["subtitle-z", "cover-a"],
            records[0]
                .output
                .resources
                .iter()
                .map(|resource| resource.resource.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            "/resources/subtitle-z",
            records[0].output.resources[0].resource.uri
        );
        assert_eq!(
            "/resources/cover-a",
            records[0].output.results[0].artifacts[0]
                .resource
                .as_ref()
                .expect("cover artifact resource should reload")
                .uri
        );

        snapshot["tasks"][0]["output"]["snapshot_id"] = serde_json::Value::String(String::new());
        fs::write(
            &path,
            serde_json::to_vec_pretty(&snapshot).expect("legacy snapshot should serialize"),
        )
        .expect("legacy snapshot should be written");
        let regenerated = TaskStateStore::new(path)
            .load()
            .expect("blank snapshot id should be repaired");
        assert_eq!(42, regenerated[0].output.revision);
        assert!(
            regenerated[0]
                .output
                .snapshot_id
                .starts_with("task-output-")
        );
    }

    #[test]
    fn round_trips_bilibili_selection_and_result_items() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let timestamp = Timestamp {
            seconds: 100,
            nanos: 0,
        };
        let selection = BilibiliTaskSelection {
            mode: 4,
            selection_ids: vec!["page:1".to_owned(), "page:2".to_owned()],
            range_start_index: 0,
            range_end_index: 0,
        };
        let playback_source = PlaybackSource {
            item_id: "bilibili-playback-result-1".to_owned(),
            variant_id: "h264".to_owned(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: "http://media.example.test:8080/hls/bilibili-playback-result-1/master.m3u8"
                .to_owned(),
            expires_at: None,
        };
        let playback_policy = BilibiliPlaybackPolicy {
            transcoding_preference: BilibiliTranscodingPreference::Force.into(),
            compatible_variant_preference: BilibiliCompatibleVariantPreference::PreferRequested
                .into(),
            weak_network_preference: BilibiliWeakNetworkPreference::HoldDowngrade.into(),
        };
        let playback_session = BilibiliPlaybackSession {
            id: "bilibili-playback-result-1".to_owned(),
            title: "Part 1".to_owned(),
            content_id: "cid-1".to_owned(),
            selected_variant_id: "h264".to_owned(),
            selected_variant: Some(BilibiliPlaybackVariant {
                id: "h264".to_owned(),
                label: "1080p".to_owned(),
                source_kind: "dash".to_owned(),
                container: "mp4".to_owned(),
                video_codec: "avc1".to_owned(),
                audio_codec: "mp4a".to_owned(),
                width: 1920,
                height: 1080,
                bitrate: 1_000_000,
                size_bytes: 10_000_000,
            }),
            variants: Vec::new(),
            transcoding_plan: Some(LanTranscodingPlan {
                state: LanTranscodingPlanState::NotRequired.into(),
                profile_id: "avplayer-h264-aac-hls-v1".to_owned(),
                reason: "Already compatible.".to_owned(),
                source_variant_id: "h264".to_owned(),
                target_container: "hls/fmp4".to_owned(),
                target_video_codec: "h264".to_owned(),
                target_audio_codec: "aac".to_owned(),
                output_protocol: PlaybackProtocol::Hls.into(),
            }),
            effective_policy: Some(playback_policy),
        };
        let result_item = BilibiliTaskResultItem {
            id: "bilibili-playback-result-1".to_owned(),
            selection_id: "page:1".to_owned(),
            title: "Part 1".to_owned(),
            subtitle: "Page 1".to_owned(),
            source_kind: "video_page".to_owned(),
            content_id: "cid-1".to_owned(),
            index: 1,
            state: TaskState::Playable.into(),
            message: "Playable online.".to_owned(),
            library_item_id: String::new(),
            playback_source: Some(playback_source.clone()),
            playback_session: Some(playback_session.clone()),
        };
        let task = Task {
            id: "bilibili-playback-task".to_owned(),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Playable.into(),
            source: "BV1persist".to_owned(),
            title: "Persisted task".to_owned(),
            progress: 0.5,
            downloaded_bytes: 0,
            total_bytes: 0,
            message: "Playable.".to_owned(),
            library_item_id: String::new(),
            created_at: Some(Timestamp {
                seconds: timestamp.seconds,
                nanos: timestamp.nanos,
            }),
            updated_at: Some(timestamp),
            finished_at: None,
            playback_source: Some(playback_source.clone()),
            playback_session: Some(playback_session.clone()),
            bilibili_selection: Some(selection.clone()),
            result_items: vec![result_item.clone()],
            output_summary: None,
        };
        let output = TaskOutputRecord::from_legacy_task(&task);
        TaskStateStore::new(path.clone())
            .save(&[PersistedTaskRecord {
                task,
                options: Some(BilibiliDownloadOptions {
                    quality_preference: "1080p".to_owned(),
                    encoding_preference: String::new(),
                    prefer_tv_api: false,
                    download_subtitles: true,
                    download_danmaku: true,
                    audio_language: "ja-jp".to_owned(),
                    subtitle_ai_policy: BilibiliSubtitleAiPolicy::PreferNonAi.into(),
                    download_cover: true,
                    danmaku_formats: vec![BilibiliDanmakuFormat::Ass.into()],
                }),
                playback_options: Some(BilibiliPlaybackOptions {
                    quality_preference: "720p".to_owned(),
                    encoding_preference: "h264".to_owned(),
                    prefer_tv_api: false,
                    audio_language: "ja-jp".to_owned(),
                    playback_policy: Some(playback_policy),
                }),
                output: output.clone(),
            }])
            .expect("task state should persist");

        let records = TaskStateStore::new(path)
            .load()
            .expect("task state should reload");

        assert_eq!(1, records.len());
        assert_eq!(output, records[0].output);
        assert_eq!(Some(selection), records[0].task.bilibili_selection);
        assert_eq!(vec![result_item], records[0].task.result_items);
        assert_eq!(
            Some(LanTranscodingPlanState::NotRequired),
            records[0]
                .task
                .playback_session
                .as_ref()
                .and_then(|session| session.transcoding_plan.as_ref())
                .map(|plan| plan.state())
        );
        let options = records[0]
            .options
            .as_ref()
            .expect("download options should round-trip");
        assert_eq!("ja-jp", options.audio_language);
        assert_eq!(
            BilibiliSubtitleAiPolicy::PreferNonAi,
            options.subtitle_ai_policy()
        );
        assert!(options.download_cover);
        assert_eq!(
            vec![i32::from(BilibiliDanmakuFormat::Ass)],
            options.danmaku_formats
        );

        let playback_options = records[0]
            .playback_options
            .as_ref()
            .expect("playback options should round-trip");
        assert_eq!("ja-jp", playback_options.audio_language);
        assert_eq!(
            Some(BilibiliTranscodingPreference::Force),
            playback_options
                .playback_policy
                .as_ref()
                .map(|policy| policy.transcoding_preference())
        );
        assert_eq!(
            Some(BilibiliWeakNetworkPreference::HoldDowngrade),
            records[0]
                .task
                .playback_session
                .as_ref()
                .and_then(|session| session.effective_policy.as_ref())
                .map(|policy| policy.weak_network_preference())
        );
    }
}
