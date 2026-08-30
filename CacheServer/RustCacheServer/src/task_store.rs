use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    ffi::OsStr,
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    marker::PhantomData,
    path::{Component, Path, PathBuf},
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
    ser::{Error as SerializeError, SerializeSeq, SerializeStruct},
};

use crate::generated::tvos_net_player::v1::{
    BilibiliApiMode, BilibiliContentIdentity as ProtoBilibiliContentIdentity, BilibiliDownloadMode,
    BilibiliDownloadOptions, BilibiliPlaybackOptions, BilibiliPlaybackSession,
    BilibiliPlaybackVariant, BilibiliRequestContext, BilibiliTaskResultDetails,
    BilibiliTaskResultItem, BilibiliTaskSelection, CacheResourceRef, LanTranscodingPlan,
    PlaybackSource, Task, TaskArtifact, TaskKind, TaskProblem, TaskResult, TaskResultProgress,
    TaskResultProviderDetails, TaskResultSubject, TaskState, task_result_provider_details,
};
use crate::playback_policy::PlaybackPolicy;
use crate::task_output::{
    MAX_REGISTERED_TASK_RESOURCES, MAX_TASK_ARTIFACTS, MAX_TASK_RESOURCES, MAX_TASK_RESULTS,
    TaskOutputRecord, TaskResourceRecord,
};
use crate::{
    bilibili_playback::{BilibiliContentIdentity, BilibiliContentKind},
    bilibili_resolution::{BilibiliTaskCandidateRecord, MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES},
};

const LEGACY_TASK_STATE_SCHEMA_VERSION: u32 = 1;
const GENERIC_TASK_OUTPUT_STATE_SCHEMA_VERSION: u32 = 2;
const BILIBILI_CANDIDATE_TASK_STATE_SCHEMA_VERSION: u32 = 3;
const BILIBILI_REQUEST_CONTEXT_TASK_STATE_SCHEMA_VERSION: u32 = 4;
const TASK_STATE_SCHEMA_VERSION: u32 = 5;
const MAX_TASK_STATE_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_PERSISTED_TASKS: usize = 10_000;
pub(crate) const MAX_PERSISTED_FILE_CLEANUP_INTENTS: usize = 100_000;
const MAX_PERSISTED_BILIBILI_VARIANTS: usize = 10_000;
const MAX_PERSISTED_DANMAKU_FORMATS: usize = 16;
const MAX_PERSISTED_BILIBILI_PROFILE_ID_BYTES: usize = 256;
// A local library item ID base64-encodes the complete bounded relative path.
const MAX_PERSISTED_CLEANUP_OWNER_ID_BYTES: usize = 8_192;
const MAX_PERSISTED_CLEANUP_RELATIVE_PATH_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersistedFileCleanupKind {
    BilibiliOwnedOutputDirectory,
    BilibiliTransientOutput,
    LocalLibraryItem,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct PersistedFileCleanupIntent {
    pub(crate) kind: PersistedFileCleanupKind,
    pub(crate) owner_id: String,
    pub(crate) relative_path: String,
}

impl PersistedFileCleanupIntent {
    pub(crate) fn new(
        kind: PersistedFileCleanupKind,
        owner_id: impl Into<String>,
        relative_path: impl Into<String>,
    ) -> io::Result<Self> {
        let intent = Self {
            kind,
            owner_id: owner_id.into(),
            relative_path: relative_path.into(),
        };
        intent.validate()?;
        Ok(intent)
    }

    fn validate(&self) -> io::Result<()> {
        if self.owner_id.is_empty()
            || self.owner_id != self.owner_id.trim()
            || self.owner_id.len() > MAX_PERSISTED_CLEANUP_OWNER_ID_BYTES
            || self.owner_id.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted file cleanup owner id is invalid",
            ));
        }
        let relative = Path::new(&self.relative_path);
        let normalized = relative.components().collect::<PathBuf>();
        let components = relative.components().collect::<Vec<_>>();
        let targets_internal_storage = components.first().is_some_and(|component| {
            matches!(
                component,
                Component::Normal(value)
                    if value
                        .to_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(".tvos-net-player"))
            )
        });
        let valid_owned_output_directory = self.kind
            == PersistedFileCleanupKind::BilibiliOwnedOutputDirectory
            && components.last().is_some_and(
                |component| matches!(component, Component::Normal(value) if *value == OsStr::new(&self.owner_id)),
            )
            && (!targets_internal_storage
                || matches!(
                    components.as_slice(),
                    [Component::Normal(internal), Component::Normal(staging), Component::Normal(_)]
                        if *internal == OsStr::new(".tvos-net-player")
                            && *staging == OsStr::new("bbdown-staging")
                ));
        if self.relative_path.is_empty()
            || self.relative_path.len() > MAX_PERSISTED_CLEANUP_RELATIVE_PATH_BYTES
            || self.relative_path.contains('\0')
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || normalized.to_str() != Some(self.relative_path.as_str())
            || (self.kind == PersistedFileCleanupKind::BilibiliOwnedOutputDirectory
                && !valid_owned_output_directory)
            || (self.kind != PersistedFileCleanupKind::BilibiliOwnedOutputDirectory
                && targets_internal_storage)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted file cleanup relative path is invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct PersistedTaskState {
    pub(crate) records: Vec<PersistedTaskRecord>,
    pub(crate) file_cleanup_intents: Vec<PersistedFileCleanupIntent>,
}

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

    #[cfg(test)]
    pub(crate) fn load(&self) -> io::Result<Vec<PersistedTaskRecord>> {
        Ok(self.load_state()?.records)
    }

    pub(crate) fn load_state(&self) -> io::Result<PersistedTaskState> {
        let file = match File::open(self.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PersistedTaskState::default());
            }
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
            LEGACY_TASK_STATE_SCHEMA_VERSION
                | GENERIC_TASK_OUTPUT_STATE_SCHEMA_VERSION
                | BILIBILI_CANDIDATE_TASK_STATE_SCHEMA_VERSION
                | BILIBILI_REQUEST_CONTEXT_TASK_STATE_SCHEMA_VERSION
                | TASK_STATE_SCHEMA_VERSION
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
        if schema_version < TASK_STATE_SCHEMA_VERSION && !snapshot.file_cleanup_intents.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state schemas before v5 cannot contain file cleanup intents",
            ));
        }
        validate_file_cleanup_intents(&snapshot.file_cleanup_intents)?;
        let records = snapshot
            .tasks
            .into_iter()
            .map(|file| file.into_record(schema_version))
            .collect::<io::Result<Vec<_>>>()?;
        validate_registered_task_resource_count(&records)?;
        validate_unique_task_record_identities(&records)?;
        Ok(PersistedTaskState {
            records,
            file_cleanup_intents: snapshot.file_cleanup_intents,
        })
    }

    #[cfg(test)]
    pub(crate) fn save(&self, records: &[PersistedTaskRecord]) -> io::Result<TaskStateSaveOutcome> {
        self.save_with_file_cleanup_intents(records, &[])
    }

    pub(crate) fn save_with_file_cleanup_intents(
        &self,
        records: &[PersistedTaskRecord],
        file_cleanup_intents: &[PersistedFileCleanupIntent],
    ) -> io::Result<TaskStateSaveOutcome> {
        let snapshot = serialize_task_snapshot_with_file_cleanup_intents_and_limit(
            records,
            file_cleanup_intents,
            MAX_TASK_STATE_SNAPSHOT_BYTES,
        )?;
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

        let temp_path = temp_path_for(self.path());
        let mut temp_file = File::create(&temp_path)?;
        temp_file.write_all(&snapshot)?;
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

pub(crate) fn validate_unique_task_record_identities(
    records: &[PersistedTaskRecord],
) -> io::Result<()> {
    let mut task_ids = HashSet::new();
    let mut snapshot_ids = HashSet::new();
    let mut resource_ids = HashSet::new();
    for record in records {
        if !task_ids.insert(record.task.id.trim()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state contains a duplicate task id",
            ));
        }
        if !snapshot_ids.insert(record.output.snapshot_id.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state contains a duplicate task output snapshot id",
            ));
        }
        for resource in &record.output.resources {
            if !resource_ids.insert(resource.resource.id.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "task state contains a duplicate task output resource id",
                ));
            }
        }
    }
    Ok(())
}

fn validate_file_cleanup_intents(intents: &[PersistedFileCleanupIntent]) -> io::Result<()> {
    validate_collection_len(
        "persisted file cleanup intents",
        intents.len(),
        MAX_PERSISTED_FILE_CLEANUP_INTENTS,
    )?;
    let mut unique = HashSet::with_capacity(intents.len());
    for intent in intents {
        intent.validate()?;
        if !unique.insert(intent) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state contains a duplicate file cleanup intent",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn serialize_task_snapshot_with_limit<'a, I>(records: I, limit: usize) -> io::Result<Vec<u8>>
where
    I: IntoIterator<Item = &'a PersistedTaskRecord>,
{
    serialize_task_snapshot_with_file_cleanup_intents_and_limit(records, &[], limit)
}

fn serialize_task_snapshot_with_file_cleanup_intents_and_limit<'a, I>(
    records: I,
    file_cleanup_intents: &[PersistedFileCleanupIntent],
    limit: usize,
) -> io::Result<Vec<u8>>
where
    I: IntoIterator<Item = &'a PersistedTaskRecord>,
{
    validate_file_cleanup_intents(file_cleanup_intents)?;
    let snapshot = PersistedTaskSnapshotForSave {
        schema_version: TASK_STATE_SCHEMA_VERSION,
        tasks: PersistedTaskSequence::new(records.into_iter()),
        file_cleanup_intents,
    };
    let mut serialized = BoundedSnapshotWriter::new(limit);
    serde_json::to_writer_pretty(&mut serialized, &snapshot).map_err(invalid_data)?;
    serialized.write_all(b"\n")?;
    Ok(serialized.into_inner())
}

struct PersistedTaskSnapshotForSave<'a, I> {
    schema_version: u32,
    tasks: PersistedTaskSequence<I>,
    file_cleanup_intents: &'a [PersistedFileCleanupIntent],
}

impl<'a, 'b, I> Serialize for PersistedTaskSnapshotForSave<'a, I>
where
    I: Iterator<Item = &'b PersistedTaskRecord>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut snapshot = serializer.serialize_struct("PersistedTaskSnapshot", 3)?;
        snapshot.serialize_field("schema_version", &self.schema_version)?;
        snapshot.serialize_field("tasks", &self.tasks)?;
        snapshot.serialize_field("file_cleanup_intents", &self.file_cleanup_intents)?;
        snapshot.end()
    }
}

struct PersistedTaskSequence<I> {
    records: RefCell<Option<I>>,
}

impl<I> PersistedTaskSequence<I> {
    fn new(records: I) -> Self {
        Self {
            records: RefCell::new(Some(records)),
        }
    }
}

impl<'a, I> Serialize for PersistedTaskSequence<I>
where
    I: Iterator<Item = &'a PersistedTaskRecord>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let records = self
            .records
            .try_borrow_mut()
            .map_err(S::Error::custom)?
            .take()
            .ok_or_else(|| S::Error::custom("task state records were serialized more than once"))?;
        let mut validator = PersistedTaskSequenceValidator::default();
        let mut sequence = serializer.serialize_seq(None)?;
        for record in records {
            validator.validate(record).map_err(S::Error::custom)?;
            let file = PersistedTaskFile::from(record);
            file.validate_collection_limits()
                .map_err(S::Error::custom)?;
            sequence.serialize_element(&file)?;
        }
        sequence.end()
    }
}

#[derive(Default)]
struct PersistedTaskSequenceValidator<'a> {
    task_count: usize,
    resource_count: usize,
    task_ids: HashSet<&'a str>,
    snapshot_ids: HashSet<&'a str>,
    resource_ids: HashSet<&'a str>,
}

impl<'a> PersistedTaskSequenceValidator<'a> {
    fn validate(&mut self, record: &'a PersistedTaskRecord) -> io::Result<()> {
        self.task_count = self.task_count.saturating_add(1);
        validate_collection_len("persisted tasks", self.task_count, MAX_PERSISTED_TASKS)?;
        validate_collection_len(
            "Bilibili accepted task candidates",
            record.bilibili_candidates.len(),
            MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES,
        )?;
        validate_bilibili_request_context(record.request_context.as_ref())?;
        validate_executable_bilibili_v2_download_state(
            TASK_STATE_SCHEMA_VERSION,
            &record.task,
            record.options.as_ref(),
            record.request_context.as_ref(),
            &record.bilibili_candidates,
        )?;
        validate_bilibili_task_candidate_alignment(&record.task, &record.bilibili_candidates)?;
        for candidate in &record.bilibili_candidates {
            validate_bilibili_task_candidate(candidate)?;
        }
        self.resource_count = self
            .resource_count
            .saturating_add(record.output.resources.len());
        if self.resource_count > MAX_REGISTERED_TASK_RESOURCES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "task state cannot contain more than {MAX_REGISTERED_TASK_RESOURCES} registered resources"
                ),
            ));
        }
        if !self.task_ids.insert(record.task.id.trim()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state contains a duplicate task id",
            ));
        }
        if !self.snapshot_ids.insert(record.output.snapshot_id.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state contains a duplicate task output snapshot id",
            ));
        }
        for resource in &record.output.resources {
            if !self.resource_ids.insert(resource.resource.id.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "task state contains a duplicate task output resource id",
                ));
            }
        }
        Ok(())
    }
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

fn deserialize_file_cleanup_intents<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedFileCleanupIntent>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(
        deserializer,
        MAX_PERSISTED_FILE_CLEANUP_INTENTS,
        "persisted file cleanup intents",
    )
}

fn deserialize_bilibili_result_items<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedBilibiliTaskResultItem>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_TASK_RESULTS, "Bilibili task result items")
}

fn deserialize_bilibili_task_candidates<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedBilibiliTaskCandidate>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_vec(
        deserializer,
        MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES,
        "Bilibili accepted task candidates",
    )
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
    pub(crate) request_context: Option<BilibiliRequestContext>,
    pub(crate) bilibili_candidates: Vec<BilibiliTaskCandidateRecord>,
    pub(crate) output: TaskOutputRecord,
}

#[derive(Serialize, Deserialize)]
struct PersistedTaskSnapshot {
    schema_version: u32,
    #[serde(default, deserialize_with = "deserialize_persisted_tasks")]
    tasks: Vec<PersistedTaskFile>,
    #[serde(default, deserialize_with = "deserialize_file_cleanup_intents")]
    file_cleanup_intents: Vec<PersistedFileCleanupIntent>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_context: Option<PersistedBilibiliRequestContext>,
    #[serde(default)]
    bilibili_selection: Option<PersistedBilibiliTaskSelection>,
    #[serde(default, deserialize_with = "deserialize_bilibili_task_candidates")]
    bilibili_candidates: Vec<PersistedBilibiliTaskCandidate>,
    #[serde(default, deserialize_with = "deserialize_bilibili_result_items")]
    result_items: Vec<PersistedBilibiliTaskResultItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<PersistedTaskOutput>,
}

impl PersistedTaskFile {
    fn validate_collection_limits(&self) -> io::Result<()> {
        validate_collection_len(
            "Bilibili task result items",
            self.result_items.len(),
            MAX_TASK_RESULTS,
        )?;
        if let Some(selection) = self.bilibili_selection.as_ref() {
            validate_collection_len(
                "Bilibili selection ids",
                selection.selection_ids.len(),
                MAX_TASK_RESULTS,
            )?;
        }
        validate_collection_len(
            "Bilibili accepted task candidates",
            self.bilibili_candidates.len(),
            MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES,
        )?;
        if let Some(options) = self.bilibili_options.as_ref() {
            validate_collection_len(
                "Bilibili danmaku formats",
                options.danmaku_formats.len(),
                MAX_PERSISTED_DANMAKU_FORMATS,
            )?;
        }
        if let Some(session) = self.playback_session.as_ref() {
            validate_collection_len(
                "Bilibili playback variants",
                session.variants.len(),
                MAX_PERSISTED_BILIBILI_VARIANTS,
            )?;
        }
        for item in &self.result_items {
            if let Some(session) = item.playback_session.as_ref() {
                validate_collection_len(
                    "Bilibili playback variants",
                    session.variants.len(),
                    MAX_PERSISTED_BILIBILI_VARIANTS,
                )?;
            }
        }
        if let Some(output) = self.output.as_ref() {
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
            validate_collection_len("task output artifacts", artifact_count, MAX_TASK_ARTIFACTS)?;
            for result in &output.results {
                let Some(PersistedTaskResultProviderDetails::Bilibili(details)) =
                    result.provider_details.as_ref()
                else {
                    continue;
                };
                if let Some(session) = details.playback_session.as_ref() {
                    validate_collection_len(
                        "Bilibili task result playback variants",
                        session.variants.len(),
                        MAX_PERSISTED_BILIBILI_VARIANTS,
                    )?;
                }
            }
        }
        Ok(())
    }
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
            bilibili_candidates: record
                .bilibili_candidates
                .into_iter()
                .map(PersistedBilibiliTaskCandidate::from)
                .collect(),
            result_items: task
                .result_items
                .into_iter()
                .map(PersistedBilibiliTaskResultItem::from)
                .collect(),
            bilibili_options: record.options.map(PersistedBilibiliDownloadOptions::from),
            bilibili_playback_options: record
                .playback_options
                .map(PersistedBilibiliPlaybackOptions::from),
            request_context: record
                .request_context
                .map(PersistedBilibiliRequestContext::from),
            output: Some(PersistedTaskOutput::from(record.output)),
        }
    }
}

impl From<&PersistedTaskRecord> for PersistedTaskFile {
    fn from(record: &PersistedTaskRecord) -> Self {
        Self::from(record.clone())
    }
}

impl PersistedTaskFile {
    fn into_record(self, schema_version: u32) -> io::Result<PersistedTaskRecord> {
        if schema_version < BILIBILI_CANDIDATE_TASK_STATE_SCHEMA_VERSION
            && !self.bilibili_candidates.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state schemas before v3 cannot contain accepted Bilibili candidates",
            ));
        }
        if schema_version < BILIBILI_REQUEST_CONTEXT_TASK_STATE_SCHEMA_VERSION
            && self.request_context.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task state schemas before v4 cannot contain a Bilibili request context",
            ));
        }
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
        let bilibili_candidates = self
            .bilibili_candidates
            .into_iter()
            .map(BilibiliTaskCandidateRecord::try_from)
            .collect::<io::Result<Vec<_>>>()?;
        let mut options = self.bilibili_options.map(BilibiliDownloadOptions::from);
        let mut request_context = self.request_context.map(BilibiliRequestContext::from);
        migrate_legacy_executable_bilibili_v2_download_state(
            schema_version,
            &task,
            &mut options,
            &mut request_context,
            &bilibili_candidates,
        )?;
        validate_bilibili_request_context(request_context.as_ref())?;
        validate_executable_bilibili_v2_download_state(
            schema_version,
            &task,
            options.as_ref(),
            request_context.as_ref(),
            &bilibili_candidates,
        )?;
        validate_bilibili_task_candidate_alignment(&task, &bilibili_candidates)?;
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
            GENERIC_TASK_OUTPUT_STATE_SCHEMA_VERSION
            | BILIBILI_CANDIDATE_TASK_STATE_SCHEMA_VERSION
            | BILIBILI_REQUEST_CONTEXT_TASK_STATE_SCHEMA_VERSION
            | TASK_STATE_SCHEMA_VERSION => self
                .output
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("task state schema v{schema_version} task is missing output"),
                    )
                })?
                .into_output(&task)?,
            _ => unreachable!("task state schema version was validated before conversion"),
        };

        Ok(PersistedTaskRecord {
            task,
            options,
            playback_options: self
                .bilibili_playback_options
                .map(BilibiliPlaybackOptions::from),
            request_context,
            bilibili_candidates,
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
        let TaskOutputRecord {
            revision,
            snapshot_id,
            primary_result_id,
            results,
            resources,
            legacy_managed,
        } = output;
        Self {
            revision,
            snapshot_id,
            primary_result_id,
            results: if legacy_managed {
                Vec::new()
            } else {
                results.into_iter().map(PersistedTaskResult::from).collect()
            },
            resources: resources
                .into_iter()
                .map(PersistedCacheResourceRef::from)
                .collect(),
            legacy_managed,
        }
    }
}

impl PersistedTaskOutput {
    fn into_output(self, task: &Task) -> io::Result<TaskOutputRecord> {
        let PersistedTaskOutput {
            revision,
            snapshot_id,
            primary_result_id,
            results,
            resources,
            legacy_managed,
        } = self;
        let mut persisted_results = results
            .into_iter()
            .map(PersistedTaskResult::into_result)
            .collect::<io::Result<Vec<_>>>()?;
        let resources = resources
            .into_iter()
            .map(PersistedCacheResourceRef::into_record)
            .collect::<io::Result<Vec<_>>>()?;
        let results = if legacy_managed {
            if !resources.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy-managed task output cannot own resources",
                ));
            }
            let derived_results = TaskOutputRecord::from_legacy_task(task).results;
            for (persisted, derived) in persisted_results.iter_mut().zip(&derived_results) {
                if persisted.subject.is_none() {
                    persisted.subject = derived.subject.clone();
                }
                if persisted.provider_details.is_none() {
                    persisted.provider_details = derived.provider_details.clone();
                }
            }
            if !persisted_results.is_empty() && persisted_results != derived_results {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy-managed task output does not match its task state",
                ));
            }
            derived_results
        } else {
            persisted_results
        };
        TaskOutputRecord::restored(
            revision,
            snapshot_id,
            primary_result_id,
            results,
            resources,
            legacy_managed,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<PersistedTaskResultSubject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_details: Option<PersistedTaskResultProviderDetails>,
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
            subject: result.subject.map(PersistedTaskResultSubject::from),
            provider_details: result
                .provider_details
                .map(PersistedTaskResultProviderDetails::from),
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
            subject: self.subject.map(TaskResultSubject::from),
            provider_details: self.provider_details.map(TaskResultProviderDetails::from),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedTaskResultSubject {
    provider: String,
    kind: String,
    id: String,
    index: u32,
}

impl From<TaskResultSubject> for PersistedTaskResultSubject {
    fn from(subject: TaskResultSubject) -> Self {
        Self {
            provider: subject.provider,
            kind: subject.kind,
            id: subject.id,
            index: subject.index,
        }
    }
}

impl From<PersistedTaskResultSubject> for TaskResultSubject {
    fn from(subject: PersistedTaskResultSubject) -> Self {
        Self {
            provider: subject.provider,
            kind: subject.kind,
            id: subject.id,
            index: subject.index,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedTaskResultProviderDetails {
    Unspecified,
    Bilibili(Box<PersistedBilibiliTaskResultDetails>),
}

impl From<TaskResultProviderDetails> for PersistedTaskResultProviderDetails {
    fn from(details: TaskResultProviderDetails) -> Self {
        match details.details {
            Some(task_result_provider_details::Details::Bilibili(details)) => {
                Self::Bilibili(Box::new(PersistedBilibiliTaskResultDetails::from(details)))
            }
            None => Self::Unspecified,
        }
    }
}

impl From<PersistedTaskResultProviderDetails> for TaskResultProviderDetails {
    fn from(details: PersistedTaskResultProviderDetails) -> Self {
        let details = match details {
            PersistedTaskResultProviderDetails::Unspecified => None,
            PersistedTaskResultProviderDetails::Bilibili(details) => {
                Some(task_result_provider_details::Details::Bilibili(
                    BilibiliTaskResultDetails::from(*details),
                ))
            }
        };
        Self { details }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliTaskResultDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<PersistedProtoBilibiliContentIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    playback_session: Option<PersistedBilibiliPlaybackSession>,
}

impl From<BilibiliTaskResultDetails> for PersistedBilibiliTaskResultDetails {
    fn from(details: BilibiliTaskResultDetails) -> Self {
        Self {
            identity: details
                .identity
                .map(PersistedProtoBilibiliContentIdentity::from),
            playback_session: details
                .playback_session
                .map(PersistedBilibiliPlaybackSession::from),
        }
    }
}

impl From<PersistedBilibiliTaskResultDetails> for BilibiliTaskResultDetails {
    fn from(details: PersistedBilibiliTaskResultDetails) -> Self {
        Self {
            identity: details.identity.map(ProtoBilibiliContentIdentity::from),
            playback_session: details.playback_session.map(BilibiliPlaybackSession::from),
        }
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
    #[serde(default)]
    library_item_id: String,
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
            library_item_id: artifact.library_item_id,
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
            library_item_id: self.library_item_id,
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
struct PersistedBilibiliTaskCandidate {
    selection_id: String,
    title: String,
    subtitle: String,
    source_kind: String,
    content_id: String,
    identity: PersistedBilibiliContentIdentity,
    index: u32,
    duration_seconds: Option<u32>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedBilibiliContentKind {
    VideoPage,
    SeasonEpisode,
    CollectionItem,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliContentIdentity {
    kind: PersistedBilibiliContentKind,
    aid: Option<u64>,
    bvid: Option<String>,
    cid: Option<u64>,
    epid: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedProtoBilibiliContentIdentity {
    kind: i32,
    aid: u64,
    bvid: String,
    cid: u64,
    epid: u64,
}

impl From<ProtoBilibiliContentIdentity> for PersistedProtoBilibiliContentIdentity {
    fn from(identity: ProtoBilibiliContentIdentity) -> Self {
        Self {
            kind: identity.kind,
            aid: identity.aid,
            bvid: identity.bvid,
            cid: identity.cid,
            epid: identity.epid,
        }
    }
}

impl From<PersistedProtoBilibiliContentIdentity> for ProtoBilibiliContentIdentity {
    fn from(identity: PersistedProtoBilibiliContentIdentity) -> Self {
        Self {
            kind: identity.kind,
            aid: identity.aid,
            bvid: identity.bvid,
            cid: identity.cid,
            epid: identity.epid,
        }
    }
}

impl From<BilibiliTaskCandidateRecord> for PersistedBilibiliTaskCandidate {
    fn from(candidate: BilibiliTaskCandidateRecord) -> Self {
        Self {
            selection_id: candidate.selection_id,
            title: candidate.title,
            subtitle: candidate.subtitle,
            source_kind: candidate.source_kind,
            content_id: candidate.content_id,
            identity: PersistedBilibiliContentIdentity {
                kind: match candidate.identity.kind {
                    BilibiliContentKind::VideoPage => PersistedBilibiliContentKind::VideoPage,
                    BilibiliContentKind::SeasonEpisode => {
                        PersistedBilibiliContentKind::SeasonEpisode
                    }
                    BilibiliContentKind::CollectionItem => {
                        PersistedBilibiliContentKind::CollectionItem
                    }
                },
                aid: candidate.identity.aid,
                bvid: candidate.identity.bvid,
                cid: candidate.identity.cid,
                epid: candidate.identity.epid,
            },
            index: candidate.index,
            duration_seconds: candidate.duration_seconds,
        }
    }
}

impl TryFrom<PersistedBilibiliTaskCandidate> for BilibiliTaskCandidateRecord {
    type Error = io::Error;

    fn try_from(candidate: PersistedBilibiliTaskCandidate) -> Result<Self, Self::Error> {
        let identity = BilibiliContentIdentity {
            kind: match candidate.identity.kind {
                PersistedBilibiliContentKind::VideoPage => BilibiliContentKind::VideoPage,
                PersistedBilibiliContentKind::SeasonEpisode => BilibiliContentKind::SeasonEpisode,
                PersistedBilibiliContentKind::CollectionItem => BilibiliContentKind::CollectionItem,
            },
            aid: candidate.identity.aid,
            bvid: candidate.identity.bvid,
            cid: candidate.identity.cid,
            epid: candidate.identity.epid,
        };
        let candidate = Self {
            selection_id: candidate.selection_id,
            title: candidate.title,
            subtitle: candidate.subtitle,
            source_kind: candidate.source_kind,
            content_id: candidate.content_id,
            identity,
            index: candidate.index,
            duration_seconds: candidate.duration_seconds,
        };
        validate_bilibili_task_candidate(&candidate)?;
        Ok(candidate)
    }
}

fn validate_bilibili_task_candidate(candidate: &BilibiliTaskCandidateRecord) -> io::Result<()> {
    if candidate.selection_id.trim().is_empty()
        || candidate.title.trim().is_empty()
        || candidate.source_kind.trim().is_empty()
        || candidate.content_id.trim().is_empty()
        || candidate.index == 0
        || !candidate.identity.is_complete()
        || !candidate.identity.matches_content_id(&candidate.content_id)
        || candidate
            .identity
            .bvid
            .as_deref()
            .is_some_and(|bvid| bvid != bvid.trim())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted Bilibili task candidate contains an invalid required field",
        ));
    }

    Ok(())
}

fn validate_bilibili_request_context(context: Option<&BilibiliRequestContext>) -> io::Result<()> {
    let Some(context) = context else {
        return Ok(());
    };
    let profile = context.credential_profile_id.as_str();
    if BilibiliApiMode::try_from(context.api_mode).is_err()
        || profile != profile.trim()
        || profile.len() > MAX_PERSISTED_BILIBILI_PROFILE_ID_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted Bilibili request context is invalid",
        ));
    }
    Ok(())
}

fn validate_executable_bilibili_v2_download_state(
    schema_version: u32,
    task: &Task,
    options: Option<&BilibiliDownloadOptions>,
    request_context: Option<&BilibiliRequestContext>,
    candidates: &[BilibiliTaskCandidateRecord],
) -> io::Result<()> {
    if schema_version < BILIBILI_REQUEST_CONTEXT_TASK_STATE_SCHEMA_VERSION
        || candidates.is_empty()
        || task.kind() != TaskKind::BilibiliDownload
        || !matches!(task.state(), TaskState::Queued | TaskState::Running)
    {
        return Ok(());
    }

    let download_mode = options
        .map(|options| BilibiliDownloadMode::try_from(options.download_mode))
        .transpose()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted executable Bilibili v2 download mode is invalid",
            )
        })?;
    let api_mode = request_context
        .map(|context| BilibiliApiMode::try_from(context.api_mode))
        .transpose()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted executable Bilibili v2 API mode is invalid",
            )
        })?;
    if matches!(
        download_mode,
        None | Some(BilibiliDownloadMode::Unspecified)
    ) || matches!(api_mode, None | Some(BilibiliApiMode::Unspecified))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted executable Bilibili v2 download is missing concrete frozen execution state",
        ));
    }
    Ok(())
}

fn migrate_legacy_executable_bilibili_v2_download_state(
    schema_version: u32,
    task: &Task,
    options: &mut Option<BilibiliDownloadOptions>,
    request_context: &mut Option<BilibiliRequestContext>,
    candidates: &[BilibiliTaskCandidateRecord],
) -> io::Result<()> {
    if schema_version >= BILIBILI_REQUEST_CONTEXT_TASK_STATE_SCHEMA_VERSION
        || candidates.is_empty()
        || task.kind() != TaskKind::BilibiliDownload
        || !matches!(task.state(), TaskState::Queued | TaskState::Running)
    {
        return Ok(());
    }

    let options = options.get_or_insert_default();
    let mode = BilibiliDownloadMode::try_from(options.download_mode).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted legacy Bilibili v2 download mode is invalid",
        )
    })?;
    if mode == BilibiliDownloadMode::Unspecified {
        options.download_mode = BilibiliDownloadMode::All.into();
    }
    *request_context = Some(BilibiliRequestContext {
        api_mode: if options.prefer_tv_api {
            BilibiliApiMode::Tv.into()
        } else {
            BilibiliApiMode::Web.into()
        },
        credential_profile_id: String::new(),
    });
    Ok(())
}

fn validate_bilibili_task_candidate_alignment(
    task: &Task,
    candidates: &[BilibiliTaskCandidateRecord],
) -> io::Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    if !matches!(
        task.kind(),
        TaskKind::BilibiliDownload | TaskKind::BilibiliProgressivePlayback
    ) || task.bilibili_selection.is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted Bilibili v2 candidates belong only to v2 Bilibili tasks",
        ));
    }
    if candidates.len() != task.result_items.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted Bilibili task candidates do not match task result count",
        ));
    }
    let mut selection_ids = HashSet::with_capacity(candidates.len());
    for (candidate, result) in candidates.iter().zip(&task.result_items) {
        if !selection_ids.insert(candidate.selection_id.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted Bilibili task candidates contain duplicate selection identities",
            ));
        }
        if !result.selection_id.is_empty()
            || candidate.source_kind != result.source_kind
            || candidate.content_id != result.content_id
            || candidate.index != result.index
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted Bilibili task candidate does not match its task result plan",
            ));
        }
    }
    Ok(())
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<PersistedProtoBilibiliContentIdentity>,
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
            identity: item
                .identity
                .map(PersistedProtoBilibiliContentIdentity::from),
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
            identity: item.identity.map(ProtoBilibiliContentIdentity::from),
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
struct PersistedBilibiliRequestContext {
    api_mode: i32,
    credential_profile_id: String,
}

impl From<BilibiliRequestContext> for PersistedBilibiliRequestContext {
    fn from(context: BilibiliRequestContext) -> Self {
        Self {
            api_mode: context.api_mode,
            credential_profile_id: context.credential_profile_id,
        }
    }
}

impl From<PersistedBilibiliRequestContext> for BilibiliRequestContext {
    fn from(context: PersistedBilibiliRequestContext) -> Self {
        Self {
            api_mode: context.api_mode,
            credential_profile_id: context.credential_profile_id,
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
    #[serde(default)]
    download_mode: i32,
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
            download_mode: options.download_mode,
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
            download_mode: options.download_mode,
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
            library_item_id: String::new(),
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
            subject: None,
            provider_details: None,
        }
    }

    #[test]
    fn file_cleanup_intents_round_trip_and_fail_closed() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let store = TaskStateStore::new(&path);
        let intent = PersistedFileCleanupIntent::new(
            PersistedFileCleanupKind::BilibiliTransientOutput,
            "bilibili-cleanup-task",
            "Bilibili/video.zh-CN.srt",
        )
        .expect("cleanup intent should be valid");
        store
            .save_with_file_cleanup_intents(&[], std::slice::from_ref(&intent))
            .expect("cleanup intent should persist");
        assert_eq!(
            vec![intent.clone()],
            store
                .load_state()
                .expect("cleanup intent should reload")
                .file_cleanup_intents
        );

        let snapshot: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("cleanup snapshot should be readable"))
                .expect("cleanup snapshot should be valid JSON");
        let mut duplicated = snapshot.clone();
        duplicated["file_cleanup_intents"] = serde_json::Value::Array(vec![
            snapshot["file_cleanup_intents"][0].clone(),
            snapshot["file_cleanup_intents"][0].clone(),
        ]);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&duplicated).expect("duplicate fixture should serialize"),
        )
        .expect("duplicate fixture should be written");
        let duplicate_error = match store.load_state() {
            Ok(_) => panic!("duplicate cleanup intents must fail closed"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, duplicate_error.kind());

        let mut legacy_with_intent = snapshot.clone();
        legacy_with_intent["schema_version"] =
            serde_json::Value::from(BILIBILI_REQUEST_CONTEXT_TASK_STATE_SCHEMA_VERSION);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy_with_intent)
                .expect("legacy cleanup fixture should serialize"),
        )
        .expect("legacy cleanup fixture should be written");
        let legacy_error = match store.load_state() {
            Ok(_) => panic!("schemas before v5 must reject cleanup intents"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, legacy_error.kind());

        let mut legacy_without_intent = legacy_with_intent;
        legacy_without_intent["file_cleanup_intents"] = serde_json::Value::Array(Vec::new());
        fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy_without_intent)
                .expect("legacy fixture should serialize"),
        )
        .expect("legacy fixture should be written");
        assert!(
            store
                .load_state()
                .expect("schema v4 without cleanup intents should remain compatible")
                .file_cleanup_intents
                .is_empty()
        );

        assert!(
            PersistedFileCleanupIntent::new(
                PersistedFileCleanupKind::BilibiliTransientOutput,
                "task",
                "Bilibili//noncanonical.srt",
            )
            .is_err()
        );
        assert!(
            PersistedFileCleanupIntent::new(
                PersistedFileCleanupKind::BilibiliTransientOutput,
                "task",
                ".tvos-net-player/task-resources/body",
            )
            .is_err()
        );
        assert!(
            PersistedFileCleanupIntent::new(
                PersistedFileCleanupKind::BilibiliTransientOutput,
                "task",
                ".TVOS-NET-PLAYER/task-resources/body",
            )
            .is_err()
        );
        for relative_path in [
            ".tvos-net-player/bbdown-staging/bilibili-cleanup-task",
            "Bilibili/bilibili-cleanup-task",
        ] {
            PersistedFileCleanupIntent::new(
                PersistedFileCleanupKind::BilibiliOwnedOutputDirectory,
                "bilibili-cleanup-task",
                relative_path,
            )
            .expect("task-owned output directory should be valid");
        }
        for relative_path in [
            ".tvos-net-player/resources/bilibili-cleanup-task",
            ".tvos-net-player/bbdown-staging/another-task",
            ".TVOS-NET-PLAYER/bbdown-staging/bilibili-cleanup-task",
            "Bilibili/another-task",
        ] {
            assert!(
                PersistedFileCleanupIntent::new(
                    PersistedFileCleanupKind::BilibiliOwnedOutputDirectory,
                    "bilibili-cleanup-task",
                    relative_path,
                )
                .is_err()
            );
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
    fn bounded_snapshot_serialization_stops_before_polling_later_records() {
        let task = Task {
            id: "streamed-task".to_owned(),
            source: "BV1streamed".to_owned(),
            message: "x".repeat(4 * 1024),
            ..Default::default()
        };
        let first = PersistedTaskRecord {
            output: TaskOutputRecord::from_legacy_task(&task),
            task,
            options: None,
            playback_options: None,
            request_context: None,
            bilibili_candidates: Vec::new(),
        };
        let polled = Cell::new(0_usize);
        let records = std::iter::from_fn(|| {
            let next = polled.get().saturating_add(1);
            polled.set(next);
            match next {
                1 => Some(&first),
                _ => panic!("serialization must stop before polling another record"),
            }
        });

        let error = match serialize_task_snapshot_with_limit(records, 512) {
            Ok(_) => panic!("the first record must exceed the test snapshot budget"),
            Err(error) => error,
        };

        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert_eq!(1, polled.get());
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
            request_context: None,
            bilibili_candidates: Vec::new(),
        };

        let error = TaskStateStore::new(&path)
            .save(&[record])
            .expect_err("writer and loader collection limits must match");

        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert!(!path.exists());
    }

    #[test]
    fn task_state_rejects_cross_task_identity_collisions() {
        let record = |task_id: &str, resource_id: &str| {
            let task = Task {
                id: task_id.to_owned(),
                source: format!("BV1{task_id}"),
                ..Default::default()
            };
            let previous = TaskOutputRecord::from_legacy_task(&task);
            let resource = TaskResourceRecord::new(CacheResourceRef {
                id: resource_id.to_owned(),
                content_type: "text/plain".to_owned(),
                size_bytes: 4,
                size_known: true,
                ..Default::default()
            })
            .expect("test resource should be valid");
            let output = TaskOutputRecord::replace(
                Some(&previous),
                vec![TaskResult {
                    id: format!("result-{task_id}"),
                    state: TaskState::Completed.into(),
                    artifacts: vec![TaskArtifact {
                        id: format!("artifact-{task_id}"),
                        kind: TaskArtifactKind::Metadata.into(),
                        state: TaskArtifactState::Available.into(),
                        resource: Some(resource.resource.clone()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                vec![resource],
            )
            .expect("test output should be valid");
            PersistedTaskRecord {
                task,
                options: None,
                playback_options: None,
                request_context: None,
                bilibili_candidates: Vec::new(),
                output,
            }
        };

        let first = record("identity-one", "resource-one");
        let mut duplicate_task = record("identity-two", "resource-two");
        duplicate_task.task.id = format!("\u{2003}{}\u{2003}", first.task.id);
        let mut duplicate_snapshot = record("identity-two", "resource-two");
        duplicate_snapshot.output.snapshot_id = first.output.snapshot_id.clone();
        let duplicate_resource = record("identity-three", "resource-one");

        for (fixture_name, records) in [
            ("duplicate-task", vec![first.clone(), duplicate_task]),
            (
                "duplicate-snapshot",
                vec![first.clone(), duplicate_snapshot],
            ),
            (
                "duplicate-resource",
                vec![first.clone(), duplicate_resource],
            ),
        ] {
            let temp = tempfile::tempdir().expect("temp dir should be created");
            let path = temp.path().join(fixture_name).join("tasks.json");
            std::fs::create_dir_all(path.parent().unwrap())
                .expect("snapshot directory should be created");
            let snapshot = PersistedTaskSnapshot {
                schema_version: TASK_STATE_SCHEMA_VERSION,
                tasks: records
                    .iter()
                    .cloned()
                    .map(PersistedTaskFile::from)
                    .collect(),
                file_cleanup_intents: Vec::new(),
            };
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&snapshot).expect("fixture should serialize"),
            )
            .expect("fixture should be written");

            let load_error = match TaskStateStore::new(&path).load() {
                Ok(_) => panic!("colliding task identities must fail closed during load"),
                Err(error) => error,
            };
            assert_eq!(io::ErrorKind::InvalidData, load_error.kind());

            let save_path = temp.path().join(fixture_name).join("saved.json");
            let save_error = TaskStateStore::new(&save_path)
                .save(&records)
                .expect_err("colliding task identities must fail closed during save");
            assert_eq!(io::ErrorKind::InvalidData, save_error.kind());
            assert!(!save_path.exists());
        }
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
            request_context: None,
            bilibili_candidates: Vec::new(),
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
                request_context: None,
                bilibili_candidates: Vec::new(),
                output,
            })
        };
        let accepted = PersistedTaskSnapshot {
            schema_version: TASK_STATE_SCHEMA_VERSION,
            tasks: vec![
                persisted_task("budget-one", &["resource-one"]),
                persisted_task("budget-two", &["resource-two"]),
            ],
            file_cleanup_intents: Vec::new(),
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
            file_cleanup_intents: Vec::new(),
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
            subject: None,
            provider_details: None,
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
    fn migrates_v1_snapshot_to_compact_legacy_managed_output() {
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

        let records = TaskStateStore::new(&path)
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

        let expected_output = output.clone();
        TaskStateStore::new(path.clone())
            .save(&records)
            .expect("migrated task state should write back as the current schema");
        let mut persisted: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).expect("migrated task state should remain readable"),
        )
        .expect("migrated task state should remain valid JSON");
        assert_eq!(TASK_STATE_SCHEMA_VERSION, persisted["schema_version"]);
        assert_eq!(
            0,
            persisted["tasks"][0]["output"]["results"]
                .as_array()
                .expect("legacy-managed output results should be an array")
                .len(),
            "legacy result bodies must not be duplicated during v1 writeback"
        );

        let reloaded = TaskStateStore::new(&path)
            .load()
            .expect("compact legacy-managed output should reload");
        assert_eq!(expected_output, reloaded[0].output);

        persisted["schema_version"] =
            serde_json::Value::from(GENERIC_TASK_OUTPUT_STATE_SCHEMA_VERSION);
        persisted["tasks"][0]
            .as_object_mut()
            .expect("persisted task should be an object")
            .remove("bilibili_candidates");
        persisted["tasks"][0]["output"]["results"] = serde_json::to_value(
            expected_output
                .results
                .iter()
                .cloned()
                .map(PersistedTaskResult::from)
                .collect::<Vec<_>>(),
        )
        .expect("legacy output results should serialize");
        for result in persisted["tasks"][0]["output"]["results"]
            .as_array_mut()
            .expect("legacy output results should be an array")
        {
            let result = result
                .as_object_mut()
                .expect("legacy output result should be an object");
            result.remove("subject");
            result.remove("provider_details");
        }
        let mut duplicated_bytes =
            serde_json::to_vec_pretty(&persisted).expect("duplicated v2 fixture should serialize");
        duplicated_bytes.push(b'\n');
        fs::write(&path, duplicated_bytes).expect("duplicated v2 fixture should be written");
        let duplicated = TaskStateStore::new(&path)
            .load()
            .expect("the previous duplicated v2 representation should remain compatible");
        assert_eq!(expected_output, duplicated[0].output);
        assert!(duplicated[0].bilibili_candidates.is_empty());

        TaskStateStore::new(path.clone())
            .save(&duplicated)
            .expect("schema v2 task state should migrate to the current schema");
        let migrated_v2: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).expect("migrated v2 task state should remain readable"),
        )
        .expect("migrated v2 task state should remain valid JSON");
        assert_eq!(TASK_STATE_SCHEMA_VERSION, migrated_v2["schema_version"]);

        persisted["tasks"][0]["output"]["results"][0]["title"] =
            serde_json::Value::String("Tampered result".to_owned());
        fs::write(
            &path,
            serde_json::to_vec_pretty(&persisted)
                .expect("mismatched duplicated v2 fixture should serialize"),
        )
        .expect("mismatched duplicated v2 fixture should be written");
        let error = match TaskStateStore::new(path).load() {
            Ok(_) => panic!("mismatched duplicated legacy output should fail closed"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert!(
            error
                .to_string()
                .contains("legacy-managed task output does not match")
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
        assert_eq!(
            crate::generated::tvos_net_player::v1::BilibiliDownloadMode::Unspecified,
            options.download_mode()
        );

        let playback_options = records[0]
            .playback_options
            .as_ref()
            .expect("playback options should restore");
        assert_eq!("720p", playback_options.quality_preference);
        assert!(playback_options.audio_language.is_empty());
        assert!(playback_options.playback_policy.is_none());
    }

    #[test]
    fn round_trips_nested_task_output_without_persisting_resource_locations() {
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
                        library_item_id: String::new(),
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
                        library_item_id: String::new(),
                    },
                    TaskArtifact {
                        id: "artifact-media".to_owned(),
                        kind: TaskArtifactKind::Media.into(),
                        state: TaskArtifactState::Available.into(),
                        title: "Downloaded media".to_owned(),
                        format: "mp4".to_owned(),
                        language_tag: String::new(),
                        is_ai_generated: false,
                        resource: None,
                        problem: None,
                        library_item_id: "library.result-z".to_owned(),
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
                subject: Some(TaskResultSubject {
                    provider: "bilibili".to_owned(),
                    kind: "video_page".to_owned(),
                    id: "2001".to_owned(),
                    index: 1,
                }),
                provider_details: Some(TaskResultProviderDetails {
                    details: Some(task_result_provider_details::Details::Bilibili(
                        BilibiliTaskResultDetails {
                            identity: Some(ProtoBilibiliContentIdentity {
                                kind: crate::generated::tvos_net_player::v1::BilibiliContentKind::VideoPage.into(),
                                aid: 1_001,
                                bvid: "BV1nestedOutput".to_owned(),
                                cid: 2_001,
                                epid: 0,
                            }),
                            playback_session: Some(BilibiliPlaybackSession {
                                id: "session-result-z".to_owned(),
                                content_id: "2001".to_owned(),
                                effective_policy: Some(PlaybackPolicy::default().to_proto()),
                                ..Default::default()
                            }),
                        },
                    )),
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
                subject: None,
                provider_details: None,
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
                request_context: Some(BilibiliRequestContext {
                    api_mode: crate::generated::tvos_net_player::v1::BilibiliApiMode::Web.into(),
                    credential_profile_id: "profile-main".to_owned(),
                }),
                bilibili_candidates: Vec::new(),
                output: output.clone(),
            }])
            .expect("task output should persist as the current schema");

        let mut snapshot: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).expect("persisted snapshot should be readable"),
        )
        .expect("persisted snapshot should be valid JSON");
        assert_eq!(
            Some(u64::from(TASK_STATE_SCHEMA_VERSION)),
            snapshot["schema_version"].as_u64()
        );
        assert_eq!(
            Some("profile-main"),
            snapshot["tasks"][0]["request_context"]["credential_profile_id"].as_str()
        );
        assert_eq!(
            Some("bilibili"),
            snapshot["tasks"][0]["output"]["results"][0]["subject"]["provider"].as_str()
        );
        assert_eq!(
            Some(2_001),
            snapshot["tasks"][0]["output"]["results"][0]["provider_details"]["bilibili"]
                ["identity"]["cid"]
                .as_u64()
        );
        assert_eq!(
            Some("library.result-z"),
            snapshot["tasks"][0]["output"]["results"][0]["artifacts"][2]["library_item_id"]
                .as_str()
        );
        for (label, field, value) in [
            (
                "unknown API mode",
                "api_mode",
                serde_json::Value::from(9_999_i32),
            ),
            (
                "non-normalized profile id",
                "credential_profile_id",
                serde_json::Value::String(" profile-main ".to_owned()),
            ),
            (
                "oversized profile id",
                "credential_profile_id",
                serde_json::Value::String("p".repeat(MAX_PERSISTED_BILIBILI_PROFILE_ID_BYTES + 1)),
            ),
        ] {
            let mut invalid_context = snapshot.clone();
            invalid_context["tasks"][0]["request_context"][field] = value;
            fs::write(
                &path,
                serde_json::to_vec_pretty(&invalid_context)
                    .expect("invalid request context fixture should serialize"),
            )
            .expect("invalid request context fixture should be written");
            let error = match TaskStateStore::new(&path).load() {
                Ok(_) => panic!("{label}"),
                Err(error) => error,
            };
            assert_eq!(io::ErrorKind::InvalidData, error.kind());
            assert!(error.to_string().contains("request context"));
        }
        let mut v3_with_context = snapshot.clone();
        v3_with_context["schema_version"] =
            serde_json::Value::from(BILIBILI_CANDIDATE_TASK_STATE_SCHEMA_VERSION);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&v3_with_context)
                .expect("v3 context fixture should serialize"),
        )
        .expect("v3 context fixture should be written");
        let error = match TaskStateStore::new(&path).load() {
            Ok(_) => panic!("schemas before v4 must reject request context state"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert!(error.to_string().contains("before v4"));
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
            .expect("task output should reload");
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
            vec!["artifact-cover", "artifact-subtitle", "artifact-media"],
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
        assert_eq!(
            "library.result-z",
            records[0].output.results[0].artifacts[2].library_item_id
        );
        assert_eq!(
            Some("profile-main"),
            records[0]
                .request_context
                .as_ref()
                .map(|context| context.credential_profile_id.as_str())
        );
        assert_eq!(
            Some(crate::generated::tvos_net_player::v1::BilibiliApiMode::Web),
            records[0]
                .request_context
                .as_ref()
                .map(BilibiliRequestContext::api_mode)
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
    fn round_trips_accepted_bilibili_v2_candidates_and_rejects_tampering() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let result_item = BilibiliTaskResultItem {
            id: "bilibili-v2-task".to_owned(),
            selection_id: String::new(),
            title: "Part 1".to_owned(),
            subtitle: "Page 1".to_owned(),
            source_kind: "video_page".to_owned(),
            content_id: "2001".to_owned(),
            index: 1,
            state: TaskState::Preparing.into(),
            message: "Queued for playback planning.".to_owned(),
            library_item_id: String::new(),
            playback_source: None,
            playback_session: None,
            identity: Some(ProtoBilibiliContentIdentity {
                kind: crate::generated::tvos_net_player::v1::BilibiliContentKind::VideoPage.into(),
                aid: 1_001,
                bvid: "BV1stable".to_owned(),
                cid: 2_001,
                epid: 0,
            }),
        };
        let task = Task {
            id: "bilibili-v2-task".to_owned(),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Preparing.into(),
            source: "BV1stable".to_owned(),
            title: "Stable video".to_owned(),
            result_items: vec![result_item.clone()],
            ..Default::default()
        };
        let candidate = BilibiliTaskCandidateRecord {
            selection_id: "page:1:cid:2001:bvid:BV1stable:aid:1001".to_owned(),
            title: "Part 1".to_owned(),
            subtitle: "Page 1".to_owned(),
            source_kind: "video_page".to_owned(),
            content_id: "2001".to_owned(),
            identity: BilibiliContentIdentity {
                kind: BilibiliContentKind::VideoPage,
                aid: Some(1_001),
                bvid: Some("BV1stable".to_owned()),
                cid: Some(2_001),
                epid: None,
            },
            index: 1,
            duration_seconds: Some(60),
        };
        let output = TaskOutputRecord::from_legacy_task(&task);
        TaskStateStore::new(path.clone())
            .save(&[PersistedTaskRecord {
                task,
                options: None,
                playback_options: Some(BilibiliPlaybackOptions::default()),
                request_context: None,
                bilibili_candidates: vec![candidate.clone()],
                output,
            }])
            .expect("accepted candidate should persist before planning");

        let snapshot_bytes = fs::read(&path).expect("task snapshot should be readable");
        let snapshot: serde_json::Value =
            serde_json::from_slice(&snapshot_bytes).expect("task snapshot should be valid JSON");
        let persisted_candidate = snapshot["tasks"][0]["bilibili_candidates"][0]
            .as_object()
            .expect("persisted candidate should be an object");
        assert!(!persisted_candidate.contains_key("cover_uri"));
        assert_eq!(
            Some("video_page"),
            persisted_candidate["identity"]["kind"].as_str()
        );
        assert_eq!(Some(2_001), persisted_candidate["identity"]["cid"].as_u64());
        assert_eq!(
            Some(2_001),
            snapshot["tasks"][0]["result_items"][0]["identity"]["cid"].as_u64()
        );

        let records = TaskStateStore::new(&path)
            .load()
            .expect("accepted candidate should reload");
        assert_eq!(vec![candidate], records[0].bilibili_candidates);
        assert_eq!(vec![result_item], records[0].task.result_items);
        assert!(records[0].task.result_items[0].selection_id.is_empty());

        let mut v3_snapshot = snapshot.clone();
        v3_snapshot["schema_version"] =
            serde_json::Value::from(BILIBILI_CANDIDATE_TASK_STATE_SCHEMA_VERSION);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&v3_snapshot).expect("v3 fixture should serialize"),
        )
        .expect("v3 fixture should be written");
        TaskStateStore::new(&path)
            .load()
            .expect("schema v3 candidates remain readable without request context");

        let mut invalid_identity = snapshot.clone();
        invalid_identity["tasks"][0]["bilibili_candidates"][0]["identity"]["cid"] =
            serde_json::Value::from(0_u64);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&invalid_identity)
                .expect("invalid identity fixture should serialize"),
        )
        .expect("invalid identity fixture should be written");
        let error = match TaskStateStore::new(&path).load() {
            Ok(_) => panic!("an incomplete stable identity must fail closed"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, error.kind());

        let mut mismatched_identity = snapshot.clone();
        mismatched_identity["tasks"][0]["bilibili_candidates"][0]["identity"]["cid"] =
            serde_json::Value::from(2_002_u64);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&mismatched_identity)
                .expect("mismatched identity fixture should serialize"),
        )
        .expect("mismatched identity fixture should be written");
        let error = match TaskStateStore::new(&path).load() {
            Ok(_) => panic!("a stable identity must match the accepted content id"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, error.kind());

        let mut old_schema_with_candidates = snapshot.clone();
        old_schema_with_candidates["schema_version"] =
            serde_json::Value::from(GENERIC_TASK_OUTPUT_STATE_SCHEMA_VERSION);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&old_schema_with_candidates)
                .expect("old schema fixture should serialize"),
        )
        .expect("old schema fixture should be written");
        let error = match TaskStateStore::new(&path).load() {
            Ok(_) => panic!("schemas before v3 must not accept durable candidate identities"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, error.kind());

        let mut wrong_task_kind = snapshot.clone();
        wrong_task_kind["tasks"][0]["kind"] =
            serde_json::Value::from(i32::from(TaskKind::LibraryRescan));
        fs::write(
            &path,
            serde_json::to_vec_pretty(&wrong_task_kind)
                .expect("wrong task kind fixture should serialize"),
        )
        .expect("wrong task kind fixture should be written");
        let error = match TaskStateStore::new(&path).load() {
            Ok(_) => panic!("v2 candidates must remain bound to Bilibili tasks"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, error.kind());

        let mut misaligned = snapshot;
        misaligned["tasks"][0]["result_items"][0]["content_id"] =
            serde_json::Value::String("tampered-content-id".to_owned());
        fs::write(
            &path,
            serde_json::to_vec_pretty(&misaligned)
                .expect("misaligned candidate fixture should serialize"),
        )
        .expect("misaligned candidate fixture should be written");
        let error = match TaskStateStore::new(path).load() {
            Ok(_) => panic!("candidate and task result plans must stay aligned"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, error.kind());
    }

    #[test]
    fn executable_bilibili_v2_download_requires_concrete_frozen_execution_state() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let result_item = BilibiliTaskResultItem {
            id: "bilibili-v2-download".to_owned(),
            title: "Part 1".to_owned(),
            source_kind: "video_page".to_owned(),
            content_id: "2001".to_owned(),
            index: 1,
            state: TaskState::Queued.into(),
            identity: Some(ProtoBilibiliContentIdentity {
                kind: crate::generated::tvos_net_player::v1::BilibiliContentKind::VideoPage.into(),
                aid: 1_001,
                bvid: "BV1frozen".to_owned(),
                cid: 2_001,
                epid: 0,
            }),
            ..Default::default()
        };
        let task = Task {
            id: "bilibili-v2-download".to_owned(),
            kind: TaskKind::BilibiliDownload.into(),
            state: TaskState::Queued.into(),
            source: "BV1frozen".to_owned(),
            result_items: vec![result_item],
            ..Default::default()
        };
        let candidate = BilibiliTaskCandidateRecord {
            selection_id: "page:1:cid:2001:bvid:BV1frozen:aid:1001".to_owned(),
            title: "Part 1".to_owned(),
            subtitle: String::new(),
            source_kind: "video_page".to_owned(),
            content_id: "2001".to_owned(),
            identity: BilibiliContentIdentity {
                kind: BilibiliContentKind::VideoPage,
                aid: Some(1_001),
                bvid: Some("BV1frozen".to_owned()),
                cid: Some(2_001),
                epid: None,
            },
            index: 1,
            duration_seconds: Some(60),
        };
        let output = TaskOutputRecord::from_legacy_task(&task);
        TaskStateStore::new(&path)
            .save(&[PersistedTaskRecord {
                task,
                options: Some(BilibiliDownloadOptions {
                    download_mode: BilibiliDownloadMode::All.into(),
                    ..Default::default()
                }),
                playback_options: None,
                request_context: Some(BilibiliRequestContext {
                    api_mode: BilibiliApiMode::Web.into(),
                    credential_profile_id: String::new(),
                }),
                bilibili_candidates: vec![candidate],
                output,
            }])
            .expect("concrete v2 execution state should persist");

        let snapshot: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("task snapshot should be readable"))
                .expect("task snapshot should be valid JSON");
        let assert_rejected = |fixture: serde_json::Value, label: &str| {
            fs::write(
                &path,
                serde_json::to_vec_pretty(&fixture).expect("fixture should serialize"),
            )
            .expect("fixture should be written");
            let error = match TaskStateStore::new(&path).load() {
                Ok(_) => panic!("{label}"),
                Err(error) => error,
            };
            assert_eq!(io::ErrorKind::InvalidData, error.kind(), "{label}");
            assert!(
                error.to_string().contains("frozen execution state"),
                "{label}"
            );
        };

        let mut missing_options = snapshot.clone();
        missing_options["tasks"][0]
            .as_object_mut()
            .expect("task should be an object")
            .remove("bilibili_options");
        assert_rejected(missing_options, "missing options must fail closed");

        let mut missing_context = snapshot.clone();
        missing_context["tasks"][0]
            .as_object_mut()
            .expect("task should be an object")
            .remove("request_context");
        assert_rejected(missing_context, "missing context must fail closed");

        let mut unspecified_download_mode = snapshot.clone();
        unspecified_download_mode["tasks"][0]["bilibili_options"]["download_mode"] =
            serde_json::Value::from(i32::from(BilibiliDownloadMode::Unspecified));
        assert_rejected(
            unspecified_download_mode,
            "unspecified download mode must fail closed",
        );

        let mut unspecified_api_mode = snapshot.clone();
        unspecified_api_mode["tasks"][0]["request_context"]["api_mode"] =
            serde_json::Value::from(i32::from(BilibiliApiMode::Unspecified));
        assert_rejected(
            unspecified_api_mode,
            "unspecified API mode must fail closed",
        );

        let mut legacy = snapshot;
        legacy["schema_version"] =
            serde_json::Value::from(BILIBILI_CANDIDATE_TASK_STATE_SCHEMA_VERSION);
        let legacy_task = legacy["tasks"][0]
            .as_object_mut()
            .expect("legacy task should be an object");
        legacy_task.remove("bilibili_options");
        legacy_task.remove("request_context");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy).expect("legacy fixture should serialize"),
        )
        .expect("legacy fixture should be written");
        let legacy_records = TaskStateStore::new(path)
            .load()
            .expect("schema v3 candidate downloads remain explicitly legacy-compatible");
        assert_eq!(
            Some(BilibiliDownloadMode::All),
            legacy_records[0]
                .options
                .as_ref()
                .map(BilibiliDownloadOptions::download_mode)
        );
        assert_eq!(
            Some(BilibiliApiMode::Web),
            legacy_records[0]
                .request_context
                .as_ref()
                .map(BilibiliRequestContext::api_mode)
        );
        assert_eq!(
            Some(""),
            legacy_records[0]
                .request_context
                .as_ref()
                .map(|context| context.credential_profile_id.as_str())
        );
    }

    #[test]
    fn bounds_accepted_bilibili_v2_candidates_on_save_and_load() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let store = TaskStateStore::new(&path);
        let candidate = |index: u32| {
            let aid = 1_000 + u64::from(index);
            let cid = 2_000 + u64::from(index);
            let bvid = format!("BV1stable{index}");
            BilibiliTaskCandidateRecord {
                selection_id: format!("page:{index}:cid:{cid}:bvid:{bvid}:aid:{aid}"),
                title: format!("Part {index}"),
                subtitle: format!("Page {index}"),
                source_kind: "video_page".to_owned(),
                content_id: cid.to_string(),
                identity: BilibiliContentIdentity {
                    kind: BilibiliContentKind::VideoPage,
                    aid: Some(aid),
                    bvid: Some(bvid),
                    cid: Some(cid),
                    epid: None,
                },
                index,
                duration_seconds: Some(60),
            }
        };
        let result_item = |index: u32| BilibiliTaskResultItem {
            id: format!("bilibili-v2-task-{index}"),
            selection_id: String::new(),
            title: format!("Part {index}"),
            subtitle: format!("Page {index}"),
            source_kind: "video_page".to_owned(),
            content_id: (2_000 + u64::from(index)).to_string(),
            index,
            state: TaskState::Preparing.into(),
            message: "Queued for playback planning.".to_owned(),
            library_item_id: String::new(),
            playback_source: None,
            playback_session: None,
            identity: Some(ProtoBilibiliContentIdentity {
                kind: crate::generated::tvos_net_player::v1::BilibiliContentKind::VideoPage.into(),
                aid: 1_000 + u64::from(index),
                bvid: format!("BV1stable{index}"),
                cid: 2_000 + u64::from(index),
                epid: 0,
            }),
        };
        let task = Task {
            id: "bilibili-v2-task".to_owned(),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Preparing.into(),
            source: "BV1stable".to_owned(),
            title: "Stable video".to_owned(),
            result_items: vec![result_item(1)],
            ..Default::default()
        };
        store
            .save(&[PersistedTaskRecord {
                output: TaskOutputRecord::from_legacy_task(&task),
                task,
                options: None,
                playback_options: Some(BilibiliPlaybackOptions::default()),
                request_context: None,
                bilibili_candidates: vec![candidate(1)],
            }])
            .expect("a bounded v2 task should persist");

        let overflow_count = MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES + 1;
        let overflow_indices =
            1..=u32::try_from(overflow_count).expect("test candidate count should fit u32");
        let overflow_candidates = overflow_indices.clone().map(candidate).collect::<Vec<_>>();
        let overflow_task = Task {
            id: "oversized-bilibili-v2-task".to_owned(),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Preparing.into(),
            source: "BV1oversized".to_owned(),
            title: "Oversized video".to_owned(),
            result_items: overflow_indices.map(result_item).collect(),
            ..Default::default()
        };
        let error = store
            .save(&[PersistedTaskRecord {
                output: TaskOutputRecord::from_legacy_task(&overflow_task),
                task: overflow_task,
                options: None,
                playback_options: Some(BilibiliPlaybackOptions::default()),
                request_context: None,
                bilibili_candidates: overflow_candidates,
            }])
            .expect_err("save must reject a v2 task above the execution cap");
        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert!(
            error
                .to_string()
                .contains("Bilibili accepted task candidates cannot exceed 100 entries")
        );

        let mut snapshot: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).expect("the bounded snapshot should remain readable"),
        )
        .expect("the bounded snapshot should remain valid JSON");
        let persisted_candidate = snapshot["tasks"][0]["bilibili_candidates"][0].clone();
        snapshot["tasks"][0]["bilibili_candidates"] =
            serde_json::Value::Array(vec![persisted_candidate; overflow_count]);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&snapshot)
                .expect("oversized candidate fixture should serialize"),
        )
        .expect("oversized candidate fixture should be written");

        let error = match store.load() {
            Ok(_) => panic!("load must reject a v2 task above the execution cap"),
            Err(error) => error,
        };
        assert_eq!(io::ErrorKind::InvalidData, error.kind());
        assert!(
            error
                .to_string()
                .contains("Bilibili accepted task candidates cannot exceed 100 entries")
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
            identity: None,
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
                    download_mode: crate::generated::tvos_net_player::v1::BilibiliDownloadMode::All
                        .into(),
                }),
                playback_options: Some(BilibiliPlaybackOptions {
                    quality_preference: "720p".to_owned(),
                    encoding_preference: "h264".to_owned(),
                    prefer_tv_api: false,
                    audio_language: "ja-jp".to_owned(),
                    playback_policy: Some(playback_policy),
                }),
                request_context: None,
                bilibili_candidates: Vec::new(),
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
        assert_eq!(
            crate::generated::tvos_net_player::v1::BilibiliDownloadMode::All,
            options.download_mode()
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
