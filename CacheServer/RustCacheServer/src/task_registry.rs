use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use prost_types::Timestamp;
use tokio::sync::{Notify, mpsc};
use tonic::Status;
use uuid::Uuid;

use crate::{
    generated::tvos_net_player::v1::{
        BilibiliDanmakuFormat, BilibiliDownloadOptions, BilibiliPlaybackOptions,
        BilibiliPlaybackSession, BilibiliSubtitleAiPolicy, BilibiliTaskResultItem,
        BilibiliTaskSelection, PlaybackSource, Task, TaskArtifactState, TaskKind, TaskResult,
        TaskState,
    },
    hls_cache::HlsCacheStore,
    library::{
        list_optional_directory_names_no_follow_bounded, open_read_no_follow,
        remove_empty_directory_no_follow, remove_file_no_follow,
    },
    task_output::{
        MAX_REGISTERED_TASK_RESOURCES, MAX_TASK_RESOURCES, TaskOutputRecord, TaskResourceRecord,
        resource_id_is_canonical,
    },
    task_store::{
        PersistedTaskRecord, TaskStateSaveOutcome, TaskStateStore,
        validate_unique_task_record_identities,
    },
};

const QUEUED_MESSAGE: &str = "Queued for the BBDown adapter.";
const RUNNING_MESSAGE: &str = "Running Bilibili download adapter.";
const PLAYBACK_PREPARING_MESSAGE: &str = "Preparing Bilibili playback plan.";
const PLAYBACK_PLANNED_MESSAGE: &str = "Bilibili playback plan is ready.";
const PLAYBACK_PLAYABLE_MESSAGE: &str = "Bilibili playback session is playable.";
const PLAYBACK_COMPLETED_MESSAGE: &str =
    "Bilibili playback session is cached for offline playback.";
const PLAYBACK_CACHE_DELETED_MESSAGE: &str = "Bilibili playback cache was deleted.";
const REQUEUED_AFTER_RESTART_MESSAGE: &str = "Requeued after cache server restart.";
const PREPARING_INTERRUPTED_AFTER_RESTART_MESSAGE: &str =
    "Playback planning was interrupted during cache server restart.";
const PLAYABLE_EXPIRED_AFTER_RESTART_MESSAGE: &str =
    "Playback media session expired during cache server restart.";
const CANCELLED_AFTER_RESTART_MESSAGE: &str = "Cancelled during cache server restart.";
const CANCEL_REQUESTED_MESSAGE: &str = "Cancellation requested.";
const CANCELLED_BY_REQUEST_MESSAGE: &str = "Cancelled by request.";
const CANCELLED_MESSAGE: &str = "Cancelled before the download adapter started.";
pub(crate) const PLAYBACK_PLANNING_CANCELLED_MESSAGE: &str =
    "Cancelled before playback planning started.";
pub(crate) const PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE: &str =
    "Cancelled while planning Bilibili playback results.";
const WATCHER_EVENT_BUFFER_CAPACITY: usize = 128;
const DEFAULT_MAX_TERMINAL_TASKS: usize = 200;
const DEFAULT_TERMINAL_TASK_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
// Bound startup allocation from an untrusted resource directory. Exceeding the bound leaves
// bodies untouched and disables v2 resource serving until the namespace is repaired.
const MAX_TASK_RESOURCE_DIRECTORY_NAMES: usize = MAX_REGISTERED_TASK_RESOURCES + MAX_TASK_RESOURCES;

pub(crate) fn is_known_safe_cancellation_message(message: &str) -> bool {
    matches!(
        message,
        CANCEL_REQUESTED_MESSAGE
            | CANCELLED_BY_REQUEST_MESSAGE
            | CANCELLED_AFTER_RESTART_MESSAGE
            | CANCELLED_MESSAGE
            | PLAYBACK_PLANNING_CANCELLED_MESSAGE
            | PLAYBACK_RESULTS_PLANNING_CANCELLED_MESSAGE
            | "Cancelled before the BBDown adapter started."
            | "Cancelled while BBDown planning was running."
            | "Cancelled after Bilibili planning completed."
            | "Cancelled before the BBDown download started."
            | "Cancelled while the BBDown download was running."
            | "Cancelled before BBDown muxing started."
            | "Cancelled while BBDown muxing was running."
            | "Cancelled after BBDown muxing completed."
            | "Cancelled after the BBDown download finished."
            | "Cancelled before committing the BBDown archive."
            | "Cancelled while BBDown input resolution was running."
            | "Cancelled while BBDown playback planning was running."
            | "Cancelled while BBDown collection item metadata resolution was running."
    )
}

pub struct BilibiliTaskRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    mutation_lock: Mutex<()>,
    queue_notify: Arc<Notify>,
    persistence: Option<TaskStatePersistence>,
    // Load failure suppresses the writable store but must not erase configuration intent.
    persistence_configured: bool,
    retention_policy: TaskRetentionPolicy,
    resource_root_path: Option<PathBuf>,
    resource_cleanup_lock: Mutex<()>,
    resource_storage_available: AtomicBool,
    orphan_resource_scan_pending: AtomicBool,
}

#[allow(dead_code)]
pub(crate) struct StagedTaskOutputReplacement<'a> {
    registry: &'a BilibiliTaskRegistry,
    task_id: String,
    resources: Option<Vec<TaskResourceRecord>>,
    resource_ids: Option<HashSet<String>>,
    resource_body_creation_ids: HashSet<String>,
}

#[allow(dead_code)]
impl<'a> StagedTaskOutputReplacement<'a> {
    fn new(
        registry: &'a BilibiliTaskRegistry,
        task_id: String,
        resources: Vec<TaskResourceRecord>,
        resource_ids: HashSet<String>,
        resource_body_creation_ids: HashSet<String>,
    ) -> Self {
        Self {
            registry,
            task_id,
            resources: Some(resources),
            resource_ids: Some(resource_ids),
            resource_body_creation_ids,
        }
    }

    pub(crate) fn resources_requiring_body_creation(
        &self,
    ) -> impl Iterator<Item = &TaskResourceRecord> {
        self.resources
            .as_deref()
            .expect("staged task output resources must remain available before commit")
            .iter()
            .filter(|resource| {
                self.resource_body_creation_ids
                    .contains(&resource.resource.id)
            })
    }

    pub(crate) fn commit(mut self, results: Vec<TaskResult>) -> Result<Task, Status> {
        let resources = self
            .resources
            .take()
            .expect("staged task output resources must be committed once");
        let outcome = self.registry.commit_staged_task_output(
            &self.task_id,
            results,
            resources,
            self.resource_ids
                .as_ref()
                .expect("staged task output resource ids must remain registered"),
        );
        if outcome.is_ok() {
            self.disarm();
        }
        outcome
    }

    fn disarm(&mut self) {
        if let Some(resource_ids) = self.resource_ids.take() {
            self.registry
                .release_staged_task_output_resources(&resource_ids);
        }
    }
}

impl Drop for StagedTaskOutputReplacement<'_> {
    fn drop(&mut self) {
        if let Some(resource_ids) = self.resource_ids.take() {
            self.registry
                .reject_staged_task_output_resources(resource_ids);
        }
    }
}

impl BilibiliTaskRegistry {
    pub fn with_persistence_path(path: impl Into<PathBuf>) -> Self {
        Self::with_persistence_path_and_retention(path, TaskRetentionPolicy::default())
    }

    pub fn with_persistence_path_and_retention(
        path: impl Into<PathBuf>,
        retention_policy: TaskRetentionPolicy,
    ) -> Self {
        Self::with_persistence_path_retention_and_resource_root(path, retention_policy, None)
    }

    pub(crate) fn with_persistence_path_retention_and_resource_root(
        path: impl Into<PathBuf>,
        retention_policy: TaskRetentionPolicy,
        resource_root_path: Option<PathBuf>,
    ) -> Self {
        let store = TaskStateStore::new(path);
        let records = match store.load() {
            Ok(records) => records,
            Err(error) => {
                eprintln!(
                    "Failed to load persisted Bilibili task state from {}; task state writeback is disabled for this process; repair the snapshot and restart the cache server: {error}",
                    store.path().display()
                );
                return Self::from_persisted_records(
                    Vec::new(),
                    None,
                    true,
                    retention_policy,
                    resource_root_path,
                )
                .expect("empty persisted task records should be valid");
            }
        };
        let registry = match Self::from_persisted_records(
            records,
            Some(store.clone()),
            true,
            retention_policy.clone(),
            resource_root_path.clone(),
        ) {
            Ok(registry) => registry,
            Err(error) => {
                eprintln!(
                    "Failed to restore persisted Bilibili task state from {}; task state writeback is disabled for this process; repair the snapshot and restart the cache server: {error}",
                    store.path().display()
                );
                return Self::from_persisted_records(
                    Vec::new(),
                    None,
                    true,
                    retention_policy,
                    resource_root_path,
                )
                .expect("empty persisted task records should be valid");
            }
        };
        registry.persist_current_state();
        registry.retire_expired_task_resources();
        registry
    }

    pub fn persistence_available(&self) -> bool {
        self.persistence
            .as_ref()
            .is_some_and(TaskStatePersistence::is_available)
    }

    pub(crate) fn persistence_configured(&self) -> bool {
        self.persistence_configured
    }

    pub(crate) fn persistence_recovery_supported(&self) -> bool {
        self.persistence.is_some()
    }

    #[cfg(test)]
    pub(crate) fn fail_next_persistence_directory_sync(&self) {
        self.persistence
            .as_ref()
            .expect("test registry must have persistence")
            .store
            .fail_next_directory_sync();
    }

    #[cfg(test)]
    pub(crate) fn block_resource_cleanup_for_test(
        &self,
        ready: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) {
        let _guard = self
            .resource_cleanup_lock
            .lock()
            .expect("task resource cleanup lock poisoned");
        ready.send(()).expect("test should observe cleanup lock");
        release
            .recv_timeout(Duration::from_secs(2))
            .expect("test should release cleanup lock");
    }

    #[cfg(test)]
    pub(crate) fn block_next_persistence_save(
        &self,
        entered: Arc<std::sync::Barrier>,
        resume: Arc<std::sync::Barrier>,
    ) {
        self.persistence
            .as_ref()
            .expect("test registry must have persistence")
            .store
            .block_next_save(entered, resume);
    }

    pub(crate) fn task_output_v2_available(&self) -> bool {
        self.resource_storage_available
            .load(AtomicOrdering::Acquire)
            && self.persistence_available()
    }

    pub fn create_bilibili_task(
        &self,
        source: &str,
        options: Option<BilibiliDownloadOptions>,
    ) -> Result<Task, Status> {
        let normalized_source = normalize(source);
        if normalized_source.is_empty() {
            return Err(Status::invalid_argument("Bilibili URL or id is required."));
        }

        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let active_key = ActiveBilibiliTaskKey::download(&normalized_source, options.as_ref());
        if let Some(active_task_id) = inner.active_task_ids_by_key.get(&active_key)
            && let Some(active_task) = inner.tasks_by_id.get(active_task_id)
            && is_active(active_task.state())
        {
            return Ok(active_task.clone());
        }
        let durability_required = self.persistence.is_some();
        let checkpoint = durability_required.then(|| RegistryMutationCheckpoint::capture(&inner));

        let now = current_timestamp();
        let mut task = Task {
            id: format!("bilibili-{}", Uuid::new_v4().simple()),
            kind: TaskKind::BilibiliDownload.into(),
            state: TaskState::Queued.into(),
            source: normalized_source.clone(),
            title: String::new(),
            progress: 0.0,
            downloaded_bytes: 0,
            total_bytes: 0,
            message: QUEUED_MESSAGE.to_owned(),
            library_item_id: String::new(),
            created_at: Some(copy_timestamp(&now)),
            updated_at: Some(now),
            finished_at: None,
            playback_source: None,
            playback_session: None,
            bilibili_selection: None,
            result_items: Vec::new(),
            output_summary: None,
        };
        let output = TaskOutputRecord::from_legacy_task(&task);
        task.output_summary = Some(output.summary());

        inner
            .active_task_ids_by_key
            .insert(active_key, task.id.clone());
        inner
            .download_options_by_id
            .insert(task.id.clone(), options.clone());
        inner.queued_task_ids.push_back(task.id.clone());
        inner.outputs_by_task_id.insert(task.id.clone(), output);
        inner.tasks_by_id.insert(task.id.clone(), task.clone());
        Self::stage_publication_locked(&mut inner, task.clone());
        let outcome = self.persist_and_publish_pending(inner, durability_required, checkpoint);
        if !outcome.is_committed() {
            return Err(Status::unavailable(
                "Task creation could not be persisted durably.",
            ));
        }
        self.queue_notify.notify_one();
        Ok(task)
    }

    pub fn create_bilibili_playback_task(
        &self,
        source: &str,
        options: Option<BilibiliPlaybackOptions>,
        selection: Option<BilibiliTaskSelection>,
    ) -> Result<BilibiliPlaybackTaskCreation, Status> {
        let normalized_source = normalize(source);
        if normalized_source.is_empty() {
            return Err(Status::invalid_argument("Bilibili URL or id is required."));
        }

        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let durability_required = self.persistence.is_some();
        let checkpoint = durability_required.then(|| RegistryMutationCheckpoint::capture(&inner));
        let now = current_timestamp();
        let mut task = Task {
            id: format!("bilibili-playback-{}", Uuid::new_v4().simple()),
            kind: TaskKind::BilibiliProgressivePlayback.into(),
            state: TaskState::Preparing.into(),
            source: normalized_source.clone(),
            title: String::new(),
            progress: 0.0,
            downloaded_bytes: 0,
            total_bytes: 0,
            message: PLAYBACK_PREPARING_MESSAGE.to_owned(),
            library_item_id: String::new(),
            created_at: Some(copy_timestamp(&now)),
            updated_at: Some(now),
            finished_at: None,
            playback_source: None,
            playback_session: None,
            bilibili_selection: selection,
            result_items: Vec::new(),
            output_summary: None,
        };
        let output = TaskOutputRecord::from_legacy_task(&task);
        task.output_summary = Some(output.summary());

        inner
            .playback_options_by_id
            .insert(task.id.clone(), options.clone());
        let cancellation = BilibiliTaskCancellation::default();
        inner
            .planning_cancellations_by_id
            .insert(task.id.clone(), cancellation.clone());
        inner.outputs_by_task_id.insert(task.id.clone(), output);
        inner.tasks_by_id.insert(task.id.clone(), task.clone());
        Self::stage_publication_locked(&mut inner, task.clone());
        let outcome = self.persist_and_publish_pending(inner, durability_required, checkpoint);
        if !outcome.is_committed() {
            return Err(Status::unavailable(
                "Playback task creation could not be persisted durably.",
            ));
        }
        Ok(BilibiliPlaybackTaskCreation {
            task,
            created: true,
            cancellation: Some(cancellation),
        })
    }

    pub fn get_task(&self, id: &str) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner
            .visible_tasks_by_id
            .get(&normalized_id)
            .cloned()
            .ok_or_else(task_not_found)
    }

    #[cfg(test)]
    pub(crate) fn task_output_snapshot(&self, id: &str) -> Result<TaskOutputSnapshot, Status> {
        let normalized_id = normalize_required_id(id)?;
        let inner = self.inner.lock().expect("task registry lock poisoned");
        if !inner.visible_tasks_by_id.contains_key(&normalized_id) {
            return Err(task_not_found());
        }
        let output = inner
            .visible_outputs_by_task_id
            .get(&normalized_id)
            .expect("known task must have output");
        Ok(TaskOutputSnapshot {
            task_id: normalized_id,
            revision: output.record.revision,
            snapshot_id: output.record.snapshot_id.clone(),
            resource_lease_id: String::new(),
            resource_lease_expires_at: Instant::now(),
            encoded_bytes: output.encoded_bytes,
            output: Arc::clone(output),
        })
    }

    pub(crate) fn retain_task_output_snapshot(
        &self,
        id: &str,
        expires_at: Instant,
    ) -> Result<TaskOutputSnapshot, Status> {
        self.retire_expired_task_resources();
        let normalized_id = normalize_required_id(id)?;
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if !self.task_output_v2_available() {
            return Err(Status::failed_precondition(
                "Durable task output is unavailable on this cache server.",
            ));
        }
        prune_expired_resource_snapshots_locked(&mut inner, Instant::now());
        if !inner.visible_tasks_by_id.contains_key(&normalized_id) {
            return Err(task_not_found());
        }
        let output = inner
            .visible_outputs_by_task_id
            .get(&normalized_id)
            .expect("known task must have output");
        let snapshot = TaskOutputSnapshot {
            task_id: normalized_id,
            revision: output.record.revision,
            snapshot_id: output.record.snapshot_id.clone(),
            resource_lease_id: format!("task-output-resource-lease-{}", Uuid::new_v4().simple()),
            resource_lease_expires_at: expires_at,
            encoded_bytes: output.encoded_bytes,
            output: Arc::clone(output),
        };
        let retained = RetainedTaskResourceSnapshot::from_output(&snapshot);
        if !retained.output.record.resources.is_empty() {
            inner
                .retained_resource_snapshots
                .insert(snapshot.resource_lease_id.clone(), retained);
        }
        drop(inner);
        self.cleanup_durable_resource_bodies();
        Ok(snapshot)
    }

    /// Claims resource IDs against cleanup before PR6D's adapter creates any body files.
    /// Bodies must be created only after this returns and while the returned claim remains alive.
    #[allow(dead_code)]
    pub(crate) fn stage_task_output_replacement(
        &self,
        id: &str,
        resources: Vec<TaskResourceRecord>,
    ) -> Result<StagedTaskOutputReplacement<'_>, Status> {
        if !self
            .resource_storage_available
            .load(AtomicOrdering::Acquire)
            && !self
                .orphan_resource_scan_pending
                .load(AtomicOrdering::Acquire)
        {
            return Err(Status::failed_precondition(
                "Task resource storage is unavailable.",
            ));
        }
        let normalized_id = normalize_required_id(id)?;
        let candidate_resource_ids = resources
            .iter()
            .map(|resource| resource.resource.id.clone())
            .collect::<HashSet<_>>();
        let resource_body_creation_ids = self.register_staged_task_output_resources(
            &normalized_id,
            &resources,
            &candidate_resource_ids,
        )?;
        let mut staged = StagedTaskOutputReplacement::new(
            self,
            normalized_id,
            resources,
            candidate_resource_ids.clone(),
            resource_body_creation_ids,
        );
        self.retire_expired_task_resources_except(&candidate_resource_ids);
        if !self
            .resource_storage_available
            .load(AtomicOrdering::Acquire)
        {
            staged.disarm();
            return Err(Status::failed_precondition(
                "Task resource storage is unavailable.",
            ));
        }
        Ok(staged)
    }

    #[allow(dead_code)]
    fn commit_staged_task_output(
        &self,
        id: &str,
        results: Vec<TaskResult>,
        resources: Vec<TaskResourceRecord>,
        candidate_resource_ids: &HashSet<String>,
    ) -> Result<Task, Status> {
        self.retire_expired_task_resources_except(candidate_resource_ids);
        if !self
            .resource_storage_available
            .load(AtomicOrdering::Acquire)
        {
            return Err(Status::failed_precondition(
                "Task resource storage is unavailable.",
            ));
        }
        let normalized_id = normalize_required_id(id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if !inner.tasks_by_id.contains_key(&normalized_id) {
            return Err(task_not_found());
        }
        prune_expired_resource_snapshots_locked(&mut inner, Instant::now());
        validate_task_output_resource_claims_locked(
            &inner,
            &normalized_id,
            &resources,
            candidate_resource_ids,
            true,
        )?;

        let output = match TaskOutputRecord::replace(
            inner.outputs_by_task_id.get(&normalized_id),
            results,
            resources,
        ) {
            Ok(output) => output,
            Err(error) => return Err(Status::invalid_argument(error.to_string())),
        };
        let checkpoint = RegistryMutationCheckpoint::capture(&inner);
        let changed = inner
            .outputs_by_task_id
            .get(&normalized_id)
            .is_none_or(|previous| previous != &output);
        if changed {
            let retained_ids = output
                .resources
                .iter()
                .map(|resource| resource.resource.id.as_str())
                .collect::<HashSet<_>>();
            let retired_ids = inner
                .outputs_by_task_id
                .get(&normalized_id)
                .into_iter()
                .flat_map(|previous| &previous.resources)
                .filter(|resource| !retained_ids.contains(resource.resource.id.as_str()))
                .map(|resource| resource.resource.id.clone())
                .collect::<Vec<_>>();
            inner.pending_resource_cleanup_ids.extend(retired_ids);
        }
        inner
            .outputs_by_task_id
            .insert(normalized_id.clone(), output);
        let summary = inner
            .outputs_by_task_id
            .get(&normalized_id)
            .expect("inserted task output must exist")
            .summary();
        inner
            .tasks_by_id
            .get_mut(&normalized_id)
            .expect("known task must exist")
            .output_summary = Some(summary);
        let requires_persistence = changed
            || !self.persistence_available()
            || inner
                .pending_publications_by_id
                .contains_key(&normalized_id);
        let task = inner
            .tasks_by_id
            .get(&normalized_id)
            .expect("known task must exist")
            .clone();
        let outcome = if requires_persistence {
            Self::stage_publication_locked(&mut inner, task.clone());
            self.persist_and_publish_pending(inner, true, Some(checkpoint))
        } else {
            drop(inner);
            PersistenceCommitOutcome::Durable
        };
        if !outcome.is_committed() {
            return Err(Status::unavailable(
                "Task output could not be persisted durably.",
            ));
        }
        Ok(task)
    }

    #[cfg(test)]
    pub(crate) fn replace_task_output(
        &self,
        id: &str,
        results: Vec<TaskResult>,
        resources: Vec<TaskResourceRecord>,
    ) -> Result<Task, Status> {
        self.stage_task_output_replacement(id, resources)?
            .commit(results)
    }

    #[cfg(test)]
    pub(crate) fn task_resource(&self, id: &str) -> Option<TaskResourceRecord> {
        self.retire_expired_task_resources();
        let normalized_id = normalize(id).to_ascii_lowercase();
        if normalized_id.is_empty() {
            return None;
        }
        let now = current_timestamp();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        prune_expired_resource_snapshots_locked(&mut inner, Instant::now());
        let record = inner
            .visible_outputs_by_task_id
            .values()
            .find_map(|output| output.available_resources_by_id.get(&normalized_id))
            .cloned()
            .or_else(|| {
                inner
                    .retained_resource_snapshots
                    .values()
                    .find_map(|snapshot| {
                        snapshot
                            .output
                            .available_resources_by_id
                            .get(&normalized_id)
                    })
                    .cloned()
            })
            .filter(|record| {
                record
                    .resource
                    .expires_at
                    .as_ref()
                    .is_none_or(|expires_at| timestamp_nanos(expires_at) > timestamp_nanos(&now))
            });
        drop(inner);
        self.cleanup_durable_resource_bodies();
        record
    }

    pub(crate) fn open_task_resource(&self, id: &str) -> io::Result<Option<OpenedTaskResource>> {
        self.open_task_resource_with_prelock_hook(id, || {})
    }

    fn open_task_resource_with_prelock_hook(
        &self,
        id: &str,
        before_cleanup_lock: impl FnOnce(),
    ) -> io::Result<Option<OpenedTaskResource>> {
        self.retire_expired_task_resources();
        let normalized_id = normalize(id).to_ascii_lowercase();
        if normalized_id.is_empty() {
            return Ok(None);
        }
        let Some(resource_root_path) = self.resource_root_path.as_ref() else {
            return Ok(None);
        };
        before_cleanup_lock();
        let cleanup_guard = self
            .resource_cleanup_lock
            .lock()
            .expect("task resource cleanup lock poisoned");
        let now = current_timestamp();
        let record = {
            let mut inner = self.inner.lock().expect("task registry lock poisoned");
            prune_expired_resource_snapshots_locked(&mut inner, Instant::now());
            inner
                .visible_outputs_by_task_id
                .values()
                .find_map(|output| output.available_resources_by_id.get(&normalized_id))
                .cloned()
                .or_else(|| {
                    inner
                        .retained_resource_snapshots
                        .values()
                        .find_map(|snapshot| {
                            snapshot
                                .output
                                .available_resources_by_id
                                .get(&normalized_id)
                                .cloned()
                        })
                })
                .filter(|record| {
                    record
                        .resource
                        .expires_at
                        .as_ref()
                        .is_none_or(|expires_at| {
                            timestamp_nanos(expires_at) > timestamp_nanos(&now)
                        })
                })
        };
        let Some(record) = record else {
            return Ok(None);
        };

        // The cleanup lock protects object identity from authorization through the no-follow open.
        // After open, the file descriptor keeps that exact body object readable even if cleanup
        // unlinks its pathname; this does not claim to freeze in-place content mutations.
        let file = match open_read_no_follow(resource_root_path, &record.relative_path()) {
            Ok(file) => file,
            Err(error) => {
                self.mark_resource_storage_for_revalidation(&normalized_id, &error);
                return Err(error);
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                self.mark_resource_storage_for_revalidation(&normalized_id, &error);
                return Err(error);
            }
        };
        if !metadata.file_type().is_file() {
            let error = io::Error::new(
                io::ErrorKind::InvalidData,
                "task resource body is not a regular file",
            );
            self.mark_resource_storage_for_revalidation(&normalized_id, &error);
            return Err(error);
        }
        if record.resource.size_known
            && u64::try_from(record.resource.size_bytes).ok() != Some(metadata.len())
        {
            let error = io::Error::new(
                io::ErrorKind::InvalidData,
                "task resource body size does not match its durable metadata",
            );
            self.mark_resource_storage_for_revalidation(&normalized_id, &error);
            return Err(error);
        }
        if record
            .resource
            .expires_at
            .as_ref()
            .is_some_and(|expires_at| {
                timestamp_nanos(expires_at) <= timestamp_nanos(&current_timestamp())
            })
        {
            return Ok(None);
        }
        let opened = OpenedTaskResource {
            record,
            file,
            last_modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            size_bytes: metadata.len(),
        };
        self.inner
            .lock()
            .expect("task registry lock poisoned")
            .resource_storage_revalidation_ids
            .remove(&normalized_id);
        drop(cleanup_guard);
        self.cleanup_durable_resource_bodies();
        Ok(Some(opened))
    }

    pub(crate) fn release_task_output_snapshots(&self, resource_lease_ids: &[String]) {
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        for resource_lease_id in resource_lease_ids {
            inner.retained_resource_snapshots.remove(resource_lease_id);
        }
        drop(inner);
        self.cleanup_durable_resource_bodies();
    }

    pub fn cancel_task(&self, id: &str) -> Result<Task, Status> {
        self.cancel_task_with_hls_session_ids(id)
            .map(|outcome| outcome.task)
    }

    pub(crate) fn cancel_task_with_hls_session_ids(
        &self,
        id: &str,
    ) -> Result<TaskCancellationOutcome, Status> {
        let normalized_id = normalize_required_id(id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if self.persistence.is_some() && !self.persistence_available() {
            let outcome = self.persist_and_publish_pending(inner, true, None);
            if !outcome.is_durable() {
                return Err(Status::unavailable(
                    "Pending task state could not be persisted before cancellation.",
                ));
            }
            inner = self.inner.lock().expect("task registry lock poisoned");
        }
        let Some(current_state) = inner
            .visible_tasks_by_id
            .get(&normalized_id)
            .map(|task| task.state())
        else {
            return Err(task_not_found());
        };
        if is_terminal(current_state) {
            let task = inner
                .visible_tasks_by_id
                .get(&normalized_id)
                .expect("task must exist after state lookup")
                .clone();
            return Ok(TaskCancellationOutcome {
                hls_session_ids: playback_hls_session_ids(&task),
                task,
            });
        }

        if matches!(
            current_state,
            TaskState::Running | TaskState::Preparing | TaskState::CancelRequested
        ) {
            if let Some(cancellation) = inner.running_cancellations_by_id.get(&normalized_id) {
                cancellation.request_cancel();
            }
            if let Some(cancellation) = inner.planning_cancellations_by_id.get(&normalized_id) {
                cancellation.request_cancel();
            }

            if current_state != TaskState::CancelRequested {
                let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                    return Err(task_not_found());
                };
                task.state = TaskState::CancelRequested.into();
                task.message = CANCEL_REQUESTED_MESSAGE.to_owned();
                task.updated_at = Some(current_timestamp());
                let task = task.clone();
                let durability_required = self.persistence.is_some();
                let outcome =
                    self.persist_task_and_publish(inner, task.clone(), durability_required, None);
                if durability_required && !outcome.is_committed() {
                    return Err(Status::unavailable(
                        "Task cancellation could not be persisted durably.",
                    ));
                }
                return Ok(TaskCancellationOutcome {
                    hls_session_ids: playback_hls_session_ids(&task),
                    task,
                });
            }

            let task = inner
                .visible_tasks_by_id
                .get(&normalized_id)
                .expect("task must exist after state lookup")
                .clone();
            return Ok(TaskCancellationOutcome {
                hls_session_ids: playback_hls_session_ids(&task),
                task,
            });
        }

        let durability_required = self.persistence.is_some();
        let checkpoint = durability_required.then(|| RegistryMutationCheckpoint::capture(&inner));
        let hls_session_ids = inner
            .tasks_by_id
            .get(&normalized_id)
            .map(playback_hls_session_ids)
            .unwrap_or_default();
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };

            task.state = TaskState::Cancelled.into();
            task.message = CANCELLED_MESSAGE.to_owned();
            let updated_at = current_timestamp();
            task.updated_at = Some(copy_timestamp(&updated_at));
            task.finished_at = Some(updated_at);
            if task.kind() == TaskKind::BilibiliProgressivePlayback {
                task.playback_source = None;
                task.playback_session = None;
                clear_result_playback_metadata(
                    &mut task.result_items,
                    TaskState::Cancelled,
                    CANCELLED_MESSAGE,
                );
            }

            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);

        Self::clear_active_task_locked(&mut inner, &terminal_task);

        let deletes_hls_data =
            task.kind() == TaskKind::BilibiliProgressivePlayback && !hls_session_ids.is_empty();
        let outcome = if deletes_hls_data {
            self.persist_task_before_destructive_side_effect(
                inner,
                task.clone(),
                durability_required,
                checkpoint,
            )
        } else {
            self.persist_task_and_publish(inner, task.clone(), durability_required, checkpoint)
        };
        if durability_required
            && if deletes_hls_data {
                !outcome.is_durable()
            } else {
                !outcome.is_committed()
            }
        {
            return Err(Status::unavailable(
                "Task cancellation could not be persisted durably.",
            ));
        }
        Ok(TaskCancellationOutcome {
            task,
            hls_session_ids,
        })
    }

    pub async fn claim_next_bilibili_task(&self) -> BilibiliTaskWorkItem {
        loop {
            if let Some(work_item) = self.try_claim_next_bilibili_task() {
                return work_item;
            }

            self.queue_notify.notified().await;
        }
    }

    pub fn try_claim_next_bilibili_task(&self) -> Option<BilibiliTaskWorkItem> {
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        while let Some(task_id) = inner.queued_task_ids.pop_front() {
            let options = inner
                .download_options_by_id
                .get(&task_id)
                .cloned()
                .flatten();
            let cancellation = BilibiliTaskCancellation::default();
            let Some((task, work_item)) = ({
                let Some(task) = inner.tasks_by_id.get_mut(&task_id) else {
                    continue;
                };
                if task.state() != TaskState::Queued {
                    continue;
                }

                task.state = TaskState::Running.into();
                task.message = RUNNING_MESSAGE.to_owned();
                task.updated_at = Some(current_timestamp());
                let work_item = BilibiliTaskWorkItem {
                    task_id: task.id.clone(),
                    source: task.source.clone(),
                    options,
                    cancellation: cancellation.clone(),
                };
                Some((task.clone(), work_item))
            }) else {
                continue;
            };
            inner
                .running_cancellations_by_id
                .insert(task_id, cancellation);
            self.persist_task_and_publish(inner, task, false, None);
            return Some(work_item);
        }

        None
    }

    pub fn update_task_progress(&self, id: &str, progress: BilibiliTaskProgress) -> bool {
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let Some(task) = ({
            let Some(task) = inner.tasks_by_id.get_mut(id) else {
                return false;
            };
            if !matches!(
                task.state(),
                TaskState::Running | TaskState::CancelRequested
            ) {
                return false;
            }

            if let Some(value) = progress.progress
                && value.is_finite()
            {
                task.progress = value.clamp(0.0, 1.0);
            }
            if let Some(value) = progress.downloaded_bytes {
                task.downloaded_bytes = value.max(0);
            }
            if let Some(value) = progress.total_bytes {
                task.total_bytes = value.max(0);
            }
            if let Some(message) = progress.message {
                task.message = message;
            }
            task.updated_at = Some(current_timestamp());
            Some(task.clone())
        }) else {
            return false;
        };
        Self::publish_volatile_locked(&mut inner, task);
        true
    }

    pub fn update_playback_cache_progress(&self, id: &str, progress: BilibiliTaskProgress) -> bool {
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let Some(task) = ({
            let Some(task) = inner.tasks_by_id.get_mut(id) else {
                return false;
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback
                || task.state() != TaskState::Playable
            {
                return false;
            }

            if let Some(value) = progress.progress
                && value.is_finite()
            {
                task.progress = value.clamp(0.0, 1.0);
            }
            if let Some(value) = progress.downloaded_bytes {
                task.downloaded_bytes = value.max(0);
            }
            if let Some(value) = progress.total_bytes {
                task.total_bytes = value.max(0);
            }
            if let Some(message) = progress.message {
                task.message = message;
            }
            task.updated_at = Some(current_timestamp());
            Some(task.clone())
        }) else {
            return false;
        };
        Self::publish_volatile_locked(&mut inner, task);
        true
    }

    pub fn fail_hls_cache_fill_for_playback_session(
        &self,
        task_id: &str,
        session_id: &str,
        message: String,
    ) -> Result<Option<Task>, Status> {
        let normalized_task_id = normalize_required_id(task_id)?;
        let normalized_session_id = normalize_required_id(session_id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if self.persistence.is_some() && !self.persistence_available() {
            let outcome = self.persist_and_publish_pending(inner, true, None);
            if !outcome.is_durable() {
                return Err(Status::unavailable(
                    "Pending task state could not be persisted before recording HLS cache fill failure.",
                ));
            }
            inner = self.inner.lock().expect("task registry lock poisoned");
        }
        let durability_required = self.persistence.is_some();
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_task_id) else {
                return Err(task_not_found());
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }

            if task_uses_hls_session_as_primary(task, &normalized_session_id) {
                if task.state() != TaskState::Playable {
                    return Ok(None);
                }
                task.message = message;
                if mark_result_cache_fill_failed_for_session(
                    &mut task.result_items,
                    &normalized_session_id,
                    &task.message,
                ) {
                    task.progress = result_items_progress(&task.result_items);
                }
                task.updated_at = Some(current_timestamp());
            } else if matches!(task.state(), TaskState::Playable | TaskState::Completed)
                && mark_result_cache_fill_failed_for_session(
                    &mut task.result_items,
                    &normalized_session_id,
                    &message,
                )
            {
                task.progress = result_items_progress(&task.result_items);
                task.message = match task.state() {
                    TaskState::Completed => {
                        "Completed offline; some Bilibili playback results failed to cache offline."
                            .to_owned()
                    }
                    _ => "Playable online; some Bilibili playback results failed to cache offline."
                        .to_owned(),
                };
                task.updated_at = Some(current_timestamp());
            } else {
                return Ok(None);
            }

            task.clone()
        };
        let outcome = self.persist_task_and_publish(inner, task.clone(), durability_required, None);
        if durability_required && !outcome.is_durable() {
            return Err(Status::unavailable(
                "HLS cache fill failure could not be persisted durably.",
            ));
        }
        Ok(Some(task))
    }

    pub fn complete_task_succeeded(
        &self,
        id: &str,
        library_item_id: String,
        message: String,
    ) -> Result<Task, Status> {
        self.complete_task(
            id,
            TaskState::Succeeded,
            message,
            Some(library_item_id),
            Some(1.0),
        )
    }

    pub fn complete_playback_planned(
        &self,
        id: &str,
        title: String,
        playback_session: BilibiliPlaybackSession,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if is_terminal(task.state()) {
                return Ok(task.clone());
            }
            if task.state() == TaskState::CancelRequested {
                let finished_at = current_timestamp();
                task.state = TaskState::Cancelled.into();
                task.message = CANCELLED_BY_REQUEST_MESSAGE.to_owned();
                task.updated_at = Some(copy_timestamp(&finished_at));
                task.finished_at = Some(finished_at);
            } else {
                task.state = TaskState::Planned.into();
                task.title = title;
                task.message = PLAYBACK_PLANNED_MESSAGE.to_owned();
                task.progress = 0.0;
                task.playback_source = None;
                task.playback_session = Some(playback_session);
                task.updated_at = Some(current_timestamp());
            }

            task.clone()
        };
        if task.state() == TaskState::Planned {
            inner.planning_cancellations_by_id.remove(&normalized_id);
        }
        if is_terminal(task.state()) {
            let terminal_task = Self::terminal_task_locked(&inner, &task);
            Self::clear_active_task_locked(&mut inner, &terminal_task);
        }

        self.persist_task_and_publish(inner, task.clone(), false, None);
        Ok(task)
    }

    pub fn complete_playback_playable(
        &self,
        id: &str,
        title: String,
        playback_source: PlaybackSource,
        playback_session: BilibiliPlaybackSession,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if is_terminal(task.state()) {
                return Ok(task.clone());
            }
            if task.state() == TaskState::CancelRequested {
                let finished_at = current_timestamp();
                task.state = TaskState::Cancelled.into();
                task.message = CANCELLED_BY_REQUEST_MESSAGE.to_owned();
                task.updated_at = Some(copy_timestamp(&finished_at));
                task.finished_at = Some(finished_at);
            } else {
                task.state = TaskState::Playable.into();
                task.title = title;
                task.message = PLAYBACK_PLAYABLE_MESSAGE.to_owned();
                task.progress = 0.0;
                task.playback_source = Some(playback_source);
                task.playback_session = Some(playback_session);
                task.updated_at = Some(current_timestamp());
            }

            task.clone()
        };
        if task.state() == TaskState::Playable {
            inner.planning_cancellations_by_id.remove(&normalized_id);
            Self::clear_active_duplicate_key_locked(&mut inner, &task);
        }
        if is_terminal(task.state()) {
            let terminal_task = Self::terminal_task_locked(&inner, &task);
            Self::clear_active_task_locked(&mut inner, &terminal_task);
        }

        self.persist_task_and_publish(inner, task.clone(), false, None);
        Ok(task)
    }

    pub fn update_playback_results(
        &self,
        id: &str,
        title: Option<String>,
        message: String,
        progress: f64,
        result_items: Vec<BilibiliTaskResultItem>,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if is_terminal(task.state()) {
                return Ok(task.clone());
            }
            if let Some(title) = title {
                task.title = title;
            }
            task.message = message;
            if progress.is_finite() {
                task.progress = progress.clamp(0.0, 0.99);
            }
            task.result_items = result_items;
            task.updated_at = Some(current_timestamp());
            task.clone()
        };
        self.persist_task_and_publish(inner, task.clone(), false, None);
        Ok(task)
    }

    pub fn complete_playback_results_playable(
        &self,
        id: &str,
        title: String,
        message: String,
        playback_source: PlaybackSource,
        playback_session: BilibiliPlaybackSession,
        result_items: Vec<BilibiliTaskResultItem>,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if is_terminal(task.state()) {
                return Ok(task.clone());
            }
            if task.state() == TaskState::CancelRequested {
                let finished_at = current_timestamp();
                task.state = TaskState::Cancelled.into();
                task.message = CANCELLED_BY_REQUEST_MESSAGE.to_owned();
                task.playback_source = None;
                task.playback_session = None;
                clear_result_playback_metadata(
                    &mut task.result_items,
                    TaskState::Cancelled,
                    CANCELLED_BY_REQUEST_MESSAGE,
                );
                task.updated_at = Some(copy_timestamp(&finished_at));
                task.finished_at = Some(finished_at);
            } else {
                task.state = TaskState::Playable.into();
                task.title = title;
                task.message = message;
                task.progress = 0.0;
                task.playback_source = Some(playback_source);
                task.playback_session = Some(playback_session);
                task.result_items = result_items;
                task.updated_at = Some(current_timestamp());
            }

            task.clone()
        };
        if task.state() == TaskState::Playable {
            inner.planning_cancellations_by_id.remove(&normalized_id);
            Self::clear_active_duplicate_key_locked(&mut inner, &task);
        }
        if is_terminal(task.state()) {
            let terminal_task = Self::terminal_task_locked(&inner, &task);
            Self::clear_active_task_locked(&mut inner, &terminal_task);
        }

        self.persist_task_and_publish(inner, task.clone(), false, None);
        Ok(task)
    }

    pub fn complete_playback_cached(
        &self,
        id: &str,
        library_item_id: String,
    ) -> Result<Task, Status> {
        self.complete_playback_hls_session_cached(id, id, library_item_id)
    }

    pub fn complete_playback_hls_session_cached(
        &self,
        task_id: &str,
        session_id: &str,
        library_item_id: String,
    ) -> Result<Task, Status> {
        self.complete_playback_hls_session_cached_inner(task_id, session_id, library_item_id, None)
    }

    pub fn complete_playback_hls_session_cached_with_metadata(
        &self,
        task_id: &str,
        session_id: &str,
        library_item_id: String,
        completed_playback_session: BilibiliPlaybackSession,
    ) -> Result<Task, Status> {
        self.complete_playback_hls_session_cached_inner(
            task_id,
            session_id,
            library_item_id,
            Some(completed_playback_session),
        )
    }

    fn complete_playback_hls_session_cached_inner(
        &self,
        task_id: &str,
        session_id: &str,
        library_item_id: String,
        completed_playback_session: Option<BilibiliPlaybackSession>,
    ) -> Result<Task, Status> {
        let normalized_task_id = normalize_required_id(task_id)?;
        let normalized_session_id = normalize_required_id(session_id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if self.persistence.is_some() && !self.persistence_available() {
            let outcome = self.persist_and_publish_pending(inner, true, None);
            if !outcome.is_durable() {
                return Err(Status::unavailable(
                    "Pending task state could not be persisted before HLS cache completion.",
                ));
            }
            inner = self.inner.lock().expect("task registry lock poisoned");
        }
        let durability_required = self.persistence.is_some();
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_task_id) else {
                return Err(task_not_found());
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if is_terminal(task.state())
                && !completed_task_has_playable_result_session(task, &normalized_session_id)
            {
                return Ok(task.clone());
            }
            if task.playback_source.is_none() || task.playback_session.is_none() {
                return Err(Status::failed_precondition(
                    "Task does not have a playable Bilibili playback session.",
                ));
            }
            let is_primary_session = task_uses_hls_session_as_primary(task, &normalized_session_id);
            let is_result_session =
                task_has_playable_or_completed_result_session(task, &normalized_session_id);
            if !is_primary_session && !is_result_session {
                return Err(Status::failed_precondition(
                    "HLS session is not a playable task result session.",
                ));
            }

            if task.state() == TaskState::CancelRequested {
                let finished_at = current_timestamp();
                task.state = TaskState::Cancelled.into();
                task.message = CANCELLED_BY_REQUEST_MESSAGE.to_owned();
                task.library_item_id.clear();
                task.playback_source = None;
                task.playback_session = None;
                clear_result_playback_metadata(
                    &mut task.result_items,
                    TaskState::Cancelled,
                    CANCELLED_BY_REQUEST_MESSAGE,
                );
                task.updated_at = Some(copy_timestamp(&finished_at));
                task.finished_at = Some(finished_at);
            } else if is_primary_session {
                let finished_at = current_timestamp();
                task.state = TaskState::Completed.into();
                task.message = PLAYBACK_COMPLETED_MESSAGE.to_owned();
                task.library_item_id = library_item_id.clone();
                if let Some(playback_source) = task.playback_source.as_mut() {
                    playback_source.item_id = library_item_id.clone();
                    if let Some(playback_session) = &completed_playback_session {
                        playback_source.variant_id = playback_session.selected_variant_id.clone();
                    }
                    playback_source.expires_at = None;
                }
                if let Some(playback_session) = &completed_playback_session {
                    task.playback_session = Some(playback_session.clone());
                }
                for item in &mut task.result_items {
                    if result_item_uses_hls_session(item, &normalized_session_id) {
                        item.state = TaskState::Completed.into();
                        item.message = PLAYBACK_COMPLETED_MESSAGE.to_owned();
                        item.library_item_id = library_item_id.clone();
                        if let Some(playback_source) = item.playback_source.as_mut() {
                            playback_source.item_id = library_item_id.clone();
                            if let Some(playback_session) = &completed_playback_session {
                                playback_source.variant_id =
                                    playback_session.selected_variant_id.clone();
                            }
                            playback_source.expires_at = None;
                        }
                        if let Some(playback_session) = &completed_playback_session {
                            item.playback_session = Some(playback_session.clone());
                        }
                    }
                }
                task.progress = 1.0;
                task.updated_at = Some(copy_timestamp(&finished_at));
                task.finished_at = Some(finished_at);
            } else {
                for item in &mut task.result_items {
                    if result_item_uses_hls_session(item, &normalized_session_id) {
                        item.state = TaskState::Completed.into();
                        item.message = PLAYBACK_COMPLETED_MESSAGE.to_owned();
                        item.library_item_id = library_item_id.clone();
                        if let Some(playback_source) = item.playback_source.as_mut() {
                            playback_source.item_id = library_item_id.clone();
                            if let Some(playback_session) = &completed_playback_session {
                                playback_source.variant_id =
                                    playback_session.selected_variant_id.clone();
                            }
                            playback_source.expires_at = None;
                        }
                        if let Some(playback_session) = &completed_playback_session {
                            item.playback_session = Some(playback_session.clone());
                        }
                    }
                }
                task.progress = result_items_progress(&task.result_items);
                if task.state() == TaskState::Completed {
                    task.message =
                        "Completed offline; selected Bilibili playback results are cached."
                            .to_owned();
                } else {
                    task.message =
                        "Playable online; selected Bilibili playback results are cached offline."
                            .to_owned();
                }
                task.updated_at = Some(current_timestamp());
            }

            task.clone()
        };
        if is_terminal(task.state()) {
            let terminal_task = Self::terminal_task_locked(&inner, &task);
            Self::clear_active_task_locked(&mut inner, &terminal_task);
        }
        let outcome = self.persist_task_and_publish(inner, task.clone(), durability_required, None);
        if durability_required && !outcome.is_durable() {
            return Err(Status::unavailable(
                "HLS cache completion could not be persisted durably.",
            ));
        }
        Ok(task)
    }

    pub fn playable_task_id_for_hls_session(&self, session_id: &str) -> Option<String> {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner.visible_tasks_by_id.values().find_map(|task| {
            (task.kind() == TaskKind::BilibiliProgressivePlayback
                && ((task.state() == TaskState::Playable
                    && task_uses_hls_session(task, &normalized_id))
                    || completed_task_has_playable_result_session(task, &normalized_id)))
            .then(|| task.id.clone())
        })
    }

    pub fn playable_task_id_for_primary_hls_session(&self, session_id: &str) -> Option<String> {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner.visible_tasks_by_id.values().find_map(|task| {
            (task.kind() == TaskKind::BilibiliProgressivePlayback
                && task.state() == TaskState::Playable
                && task_uses_hls_session_as_primary(task, &normalized_id))
            .then(|| task.id.clone())
        })
    }

    pub fn completed_playback_task_for_hls_session(&self, session_id: &str) -> Option<Task> {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        completed_playback_task_id_for_hls_session_locked(
            &inner.visible_tasks_by_id,
            &normalized_id,
        )
        .and_then(|task_id| inner.visible_tasks_by_id.get(&task_id).cloned())
    }

    pub fn completed_playback_task_for_any_hls_session(&self, session_id: &str) -> Option<Task> {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        completed_playback_task_id_for_any_hls_session_locked(
            &inner.visible_tasks_by_id,
            &normalized_id,
        )
        .and_then(|task_id| inner.visible_tasks_by_id.get(&task_id).cloned())
    }

    pub fn playback_task_for_any_hls_session(&self, session_id: &str) -> Option<Task> {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner.visible_tasks_by_id.values().find_map(|task| {
            (task.kind() == TaskKind::BilibiliProgressivePlayback
                && matches!(task.state(), TaskState::Playable | TaskState::Completed)
                && task_uses_hls_session(task, &normalized_id))
            .then(|| task.clone())
        })
    }

    pub(crate) fn task_authorizes_hls_session_for_cleanup(&self, session_id: &str) -> bool {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() {
            return false;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner.visible_tasks_by_id.values().any(|task| {
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return false;
            }
            match task.state() {
                TaskState::Queued
                | TaskState::Running
                | TaskState::Preparing
                | TaskState::CancelRequested => {
                    task.id == normalized_id || task_uses_hls_session(task, &normalized_id)
                }
                TaskState::Playable | TaskState::Completed => {
                    task_uses_hls_session(task, &normalized_id)
                }
                _ => false,
            }
        })
    }

    pub fn completed_playback_task_matches_hls_cache_item(
        &self,
        task: &Task,
        session_id: &str,
        library_item_id: &str,
    ) -> bool {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() || normalize(library_item_id).is_empty() {
            return false;
        }
        completed_playback_task_matches_hls_cache_item(task, &normalized_id, library_item_id)
    }

    pub fn playback_task_has_completed_hls_cache_item(
        &self,
        task: &Task,
        session_id: &str,
        library_item_id: &str,
    ) -> bool {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() || normalize(library_item_id).is_empty() {
            return false;
        }
        playback_task_has_completed_hls_cache_item(task, &normalized_id, library_item_id)
    }

    pub fn refresh_playback_source(
        &self,
        id: &str,
        playback_source: PlaybackSource,
    ) -> Result<Task, Status> {
        self.refresh_hls_playback_source(id, playback_source)
    }

    pub fn refresh_hls_playback_source(
        &self,
        session_id: &str,
        playback_source: PlaybackSource,
    ) -> Result<Task, Status> {
        self.refresh_hls_playback_source_with_metadata(session_id, playback_source, None)
    }

    pub fn refresh_hls_playback_source_with_metadata(
        &self,
        session_id: &str,
        playback_source: PlaybackSource,
        playback_session: Option<BilibiliPlaybackSession>,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(session_id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task = if let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) {
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if !matches!(task.state(), TaskState::Playable | TaskState::Completed) {
                return Ok(task.clone());
            }

            task.playback_source =
                Some(primary_playback_source_for_refresh(task, &playback_source));
            if let Some(playback_session) = playback_session.as_ref()
                && task_uses_hls_session_as_primary(task, &normalized_id)
            {
                task.playback_session = Some(playback_session.clone());
            }
            refresh_result_item_playback_source(task, &normalized_id, &playback_source);
            if let Some(playback_session) = playback_session.as_ref() {
                refresh_result_item_playback_session(task, &normalized_id, playback_session);
            }
            task.clone()
        } else {
            let Some(task) = inner.tasks_by_id.values_mut().find(|task| {
                task.kind() == TaskKind::BilibiliProgressivePlayback
                    && matches!(task.state(), TaskState::Playable | TaskState::Completed)
                    && task.result_items.iter().any(|item| {
                        item.id == normalized_id
                            && result_item_can_serve_online_playback_after_task_completion(item)
                    })
            }) else {
                return Err(task_not_found());
            };

            if task_uses_hls_session_as_primary(task, &normalized_id) {
                task.playback_source =
                    Some(primary_playback_source_for_refresh(task, &playback_source));
                if let Some(playback_session) = playback_session.as_ref() {
                    task.playback_session = Some(playback_session.clone());
                }
            }
            refresh_result_item_playback_source(task, &normalized_id, &playback_source);
            if let Some(playback_session) = playback_session.as_ref() {
                refresh_result_item_playback_session(task, &normalized_id, playback_session);
            }
            task.clone()
        };
        self.persist_task_and_publish(inner, task.clone(), false, None);
        Ok(task)
    }

    pub fn hls_playback_source_uri(&self, session_id: &str) -> Option<String> {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner
            .visible_tasks_by_id
            .get(&normalized_id)
            .and_then(|task| playback_source_uri_for_session(task, &normalized_id))
            .or_else(|| {
                inner
                    .visible_tasks_by_id
                    .values()
                    .find_map(|task| playback_source_uri_for_session(task, &normalized_id))
            })
    }

    pub fn complete_task_failed(&self, id: &str, message: String) -> Result<Task, Status> {
        self.complete_task(id, TaskState::Failed, message, None, None)
    }

    pub fn fail_playback_task_after_cache_restore(
        &self,
        id: &str,
        message: String,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if is_terminal(task.state()) {
                return Ok(task.clone());
            }

            let finished_at = current_timestamp();
            task.state = TaskState::Failed.into();
            task.message = message.clone();
            task.library_item_id.clear();
            task.playback_source = None;
            task.playback_session = None;
            clear_result_playback_metadata(&mut task.result_items, TaskState::Failed, &message);
            task.updated_at = Some(copy_timestamp(&finished_at));
            task.finished_at = Some(finished_at);
            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);
        Self::clear_active_task_locked(&mut inner, &terminal_task);
        self.persist_task_and_publish(inner, task.clone(), false, None);
        Ok(task)
    }

    pub fn fail_completed_playback_task_after_cache_restore(
        &self,
        session_id: &str,
        message: String,
    ) -> Result<Task, Status> {
        let normalized_session_id = normalize_required_id(session_id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task = {
            let Some(normalized_task_id) = completed_playback_task_id_for_any_hls_session_locked(
                &inner.tasks_by_id,
                &normalized_session_id,
            ) else {
                return Err(task_not_found());
            };
            let task = inner
                .tasks_by_id
                .get_mut(&normalized_task_id)
                .expect("completed task id should resolve to a task");
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if task.state() != TaskState::Completed {
                return Ok(task.clone());
            }

            let finished_at = current_timestamp();
            task.state = TaskState::Failed.into();
            task.message = message.clone();
            task.library_item_id.clear();
            task.playback_source = None;
            task.playback_session = None;
            clear_result_playback_metadata(&mut task.result_items, TaskState::Failed, &message);
            task.updated_at = Some(copy_timestamp(&finished_at));
            task.finished_at = Some(finished_at);
            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);
        Self::clear_active_task_locked(&mut inner, &terminal_task);
        self.persist_task_and_publish(inner, task.clone(), false, None);
        Ok(task)
    }

    pub fn fail_unrestorable_playback_session_after_cache_restore(
        &self,
        session_id: &str,
        message: String,
    ) -> Result<Option<Task>, Status> {
        let normalized_session_id = normalize_required_id(session_id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if self.persistence.is_some() && !self.persistence_available() {
            let outcome = self.persist_and_publish_pending(inner, true, None);
            if !outcome.is_durable() {
                return Err(Status::unavailable(
                    "Pending task state could not be persisted before rejecting a restored HLS session.",
                ));
            }
            inner = self.inner.lock().expect("task registry lock poisoned");
        }
        let durability_required = self.persistence.is_some();
        let checkpoint = durability_required.then(|| RegistryMutationCheckpoint::capture(&inner));
        let task = {
            let Some(task) = inner.tasks_by_id.values_mut().find(|task| {
                task.kind() == TaskKind::BilibiliProgressivePlayback
                    && matches!(task.state(), TaskState::Playable | TaskState::Completed)
                    && (task_uses_hls_session_as_primary(task, &normalized_session_id)
                        || task.result_items.iter().any(|item| {
                            result_item_uses_hls_session(item, &normalized_session_id)
                                && result_item_can_serve_online_playback_after_task_completion(item)
                        }))
            }) else {
                return Ok(None);
            };

            if task_uses_hls_session_as_primary(task, &normalized_session_id) {
                let finished_at = current_timestamp();
                task.state = TaskState::Failed.into();
                task.message = message.clone();
                task.library_item_id.clear();
                task.playback_source = None;
                task.playback_session = None;
                clear_result_playback_metadata(&mut task.result_items, TaskState::Failed, &message);
                task.updated_at = Some(copy_timestamp(&finished_at));
                task.finished_at = Some(finished_at);
            } else if clear_unrestorable_result_playback_metadata_for_session(
                &mut task.result_items,
                &normalized_session_id,
                &message,
            ) {
                task.progress = result_items_progress(&task.result_items);
                task.message = match task.state() {
                    TaskState::Completed => "Completed offline cache restored; some Bilibili playback results expired after cache restore.".to_owned(),
                    _ => "Playable online; some Bilibili playback results expired after cache restore.".to_owned(),
                };
                task.updated_at = Some(current_timestamp());
            } else {
                return Ok(None);
            }

            task.clone()
        };
        if task.state() == TaskState::Failed {
            let terminal_task = Self::terminal_task_locked(&inner, &task);
            Self::clear_active_task_locked(&mut inner, &terminal_task);
        }
        let outcome = self.persist_task_before_destructive_side_effect(
            inner,
            task.clone(),
            durability_required,
            checkpoint,
        );
        if durability_required && !outcome.is_durable() {
            return Err(Status::unavailable(
                "Restored HLS session failure could not be persisted durably.",
            ));
        }
        Ok(Some(task))
    }

    pub fn remove_completed_playback_task(
        &self,
        session_id: &str,
        library_item_id: &str,
    ) -> Result<bool, Status> {
        let normalized_session_id = normalize_required_id(session_id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let normalized_task_id = if let Some(normalized_task_id) =
            completed_playback_task_id_for_any_hls_session_locked(
                &inner.tasks_by_id,
                &normalized_session_id,
            ) {
            normalized_task_id
        } else {
            if let Some(normalized_task_id) =
                playback_task_id_for_completed_result_cache_item_locked(
                    &inner.tasks_by_id,
                    &normalized_session_id,
                    library_item_id,
                )
            {
                let checkpoint = RegistryMutationCheckpoint::capture(&inner);
                let task = {
                    let task = inner
                        .tasks_by_id
                        .get_mut(&normalized_task_id)
                        .expect("playable task id should resolve to a task");
                    if !clear_unrestorable_result_playback_metadata_for_session(
                        &mut task.result_items,
                        &normalized_session_id,
                        PLAYBACK_CACHE_DELETED_MESSAGE,
                    ) {
                        return Ok(false);
                    }
                    task.progress = result_items_progress(&task.result_items);
                    task.message =
                        "Playable online; one cached Bilibili playback result was deleted."
                            .to_owned();
                    task.updated_at = Some(current_timestamp());
                    task.clone()
                };
                if let Err(error) = mark_output_playback_cache_deleted_locked(
                    &mut inner,
                    &normalized_task_id,
                    &normalized_session_id,
                    library_item_id,
                    PLAYBACK_CACHE_DELETED_MESSAGE,
                ) {
                    checkpoint.restore(&mut inner);
                    return Err(Status::internal(format!(
                        "Task cache deletion would create invalid task output: {error}"
                    )));
                }
                let durability_required = self.persistence.is_some();
                let checkpoint = durability_required.then_some(checkpoint);
                let outcome = self.persist_task_before_destructive_side_effect(
                    inner,
                    task,
                    durability_required,
                    checkpoint,
                );
                if durability_required && !outcome.is_durable() {
                    return Err(Status::unavailable(
                        "Task cache deletion could not be persisted durably.",
                    ));
                }
                return Ok(true);
            }
            let Some(task) = inner.tasks_by_id.get(&normalized_session_id) else {
                return Ok(false);
            };
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            return Err(Status::failed_precondition(
                "Only completed playback tasks matching the deleted cache item can be removed.",
            ));
        };
        let checkpoint = RegistryMutationCheckpoint::capture(&inner);
        {
            let task = inner
                .tasks_by_id
                .get_mut(&normalized_task_id)
                .expect("completed task id should resolve to a task");
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                return Err(Status::failed_precondition(
                    "Task is not a Bilibili progressive playback task.",
                ));
            }
            if task.state() != TaskState::Completed {
                return Err(Status::failed_precondition(
                    "Only completed playback tasks matching the deleted cache item can be removed.",
                ));
            }
            if task.library_item_id == library_item_id
                && task_uses_hls_session_as_primary(task, &normalized_session_id)
            {
                // Fall through to whole-task removal below.
            } else if clear_completed_result_cache_item_for_session(
                &mut task.result_items,
                &normalized_session_id,
                library_item_id,
                PLAYBACK_CACHE_DELETED_MESSAGE,
            ) {
                task.progress = result_items_progress(&task.result_items);
                task.message =
                    "Completed offline; one cached Bilibili playback result was deleted."
                        .to_owned();
                task.updated_at = Some(current_timestamp());
                let task = task.clone();
                if let Err(error) = mark_output_playback_cache_deleted_locked(
                    &mut inner,
                    &normalized_task_id,
                    &normalized_session_id,
                    library_item_id,
                    PLAYBACK_CACHE_DELETED_MESSAGE,
                ) {
                    checkpoint.restore(&mut inner);
                    return Err(Status::internal(format!(
                        "Task cache deletion would create invalid task output: {error}"
                    )));
                }
                let durability_required = self.persistence.is_some();
                let checkpoint = durability_required.then_some(checkpoint);
                let outcome = self.persist_task_before_destructive_side_effect(
                    inner,
                    task,
                    durability_required,
                    checkpoint,
                );
                if durability_required && !outcome.is_durable() {
                    return Err(Status::unavailable(
                        "Task cache deletion could not be persisted durably.",
                    ));
                }
                return Ok(true);
            } else {
                return Err(Status::failed_precondition(
                    "Only completed playback tasks matching the deleted cache item can be removed.",
                ));
            }
        }

        let mut removed_task = inner
            .tasks_by_id
            .remove(&normalized_task_id)
            .expect("task must exist after precondition checks");
        let finished_at = current_timestamp();
        removed_task.state = TaskState::Failed.into();
        removed_task.message = PLAYBACK_CACHE_DELETED_MESSAGE.to_owned();
        removed_task.library_item_id.clear();
        removed_task.playback_source = None;
        removed_task.playback_session = None;
        clear_result_playback_metadata(
            &mut removed_task.result_items,
            TaskState::Failed,
            PLAYBACK_CACHE_DELETED_MESSAGE,
        );
        removed_task.updated_at = Some(copy_timestamp(&finished_at));
        removed_task.finished_at = Some(finished_at);
        inner.download_options_by_id.remove(&normalized_task_id);
        inner.playback_options_by_id.remove(&normalized_task_id);
        inner
            .running_cancellations_by_id
            .remove(&normalized_task_id);
        inner
            .planning_cancellations_by_id
            .remove(&normalized_task_id);
        let removed_output = inner.outputs_by_task_id.remove(&normalized_task_id);
        if let Some(output) = removed_output.as_ref() {
            inner.pending_resource_cleanup_ids.extend(
                output
                    .resources
                    .iter()
                    .map(|resource| resource.resource.id.clone()),
            );
        }
        removed_task.output_summary = Some(
            TaskOutputRecord::removed_task_tombstone(&removed_task, removed_output.as_ref())
                .summary(),
        );
        let durability_required = self.persistence.is_some();
        let checkpoint = durability_required.then_some(checkpoint);
        let outcome = self.persist_task_before_destructive_side_effect(
            inner,
            removed_task,
            durability_required,
            checkpoint,
        );
        if durability_required && !outcome.is_durable() {
            return Err(Status::unavailable(
                "Task cache deletion could not be persisted durably.",
            ));
        }
        Ok(true)
    }

    pub fn complete_task_cancelled(&self, id: &str, message: String) -> Result<Task, Status> {
        self.complete_task(id, TaskState::Cancelled, message, None, None)
    }

    pub fn is_cancel_requested(&self, id: &str) -> bool {
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner
            .running_cancellations_by_id
            .get(id)
            .or_else(|| inner.planning_cancellations_by_id.get(id))
            .is_some_and(BilibiliTaskCancellation::is_cancel_requested)
    }

    pub fn is_playback_task_playable(&self, id: &str) -> bool {
        let Ok(normalized_id) = normalize_required_id(id) else {
            return false;
        };
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner
            .visible_tasks_by_id
            .get(&normalized_id)
            .is_some_and(|task| {
                task.kind() == TaskKind::BilibiliProgressivePlayback
                    && task.state() == TaskState::Playable
            })
    }

    pub fn is_primary_hls_session_playable(&self, task_id: &str, session_id: &str) -> bool {
        let normalized_task_id = normalize(task_id);
        let normalized_session_id = normalize(session_id);
        if normalized_task_id.is_empty() || normalized_session_id.is_empty() {
            return false;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner
            .visible_tasks_by_id
            .get(&normalized_task_id)
            .is_some_and(|task| {
                task.kind() == TaskKind::BilibiliProgressivePlayback
                    && task.state() == TaskState::Playable
                    && task_uses_hls_session_as_primary(task, &normalized_session_id)
            })
    }

    pub fn is_hls_session_playable_for_task(&self, task_id: &str, session_id: &str) -> bool {
        self.hls_session_publication_state(task_id, session_id)
            == HlsSessionPublicationState::Published
    }

    pub(crate) fn hls_session_publication_state(
        &self,
        task_id: &str,
        session_id: &str,
    ) -> HlsSessionPublicationState {
        let normalized_task_id = normalize(task_id);
        let normalized_session_id = normalize(session_id);
        if normalized_task_id.is_empty() || normalized_session_id.is_empty() {
            return HlsSessionPublicationState::Absent;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        if inner
            .visible_tasks_by_id
            .get(&normalized_task_id)
            .is_some_and(|task| task_has_playable_hls_session(task, &normalized_session_id))
        {
            HlsSessionPublicationState::Published
        } else if inner
            .tasks_by_id
            .get(&normalized_task_id)
            .is_some_and(|task| task_has_playable_hls_session(task, &normalized_session_id))
        {
            HlsSessionPublicationState::Pending
        } else {
            HlsSessionPublicationState::Absent
        }
    }

    pub(crate) fn retry_pending_persistence(&self) -> bool {
        self.persist_current_state()
    }

    pub fn hls_session_has_online_playback_after_cache_fill_failure(
        &self,
        task_id: &str,
        session_id: &str,
    ) -> bool {
        let normalized_task_id = normalize(task_id);
        let normalized_session_id = normalize(session_id);
        if normalized_task_id.is_empty() || normalized_session_id.is_empty() {
            return false;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner
            .tasks_by_id
            .get(&normalized_task_id)
            .is_some_and(|task| {
                task.kind() == TaskKind::BilibiliProgressivePlayback
                    && (task_has_online_playback_after_cache_fill_failure(
                        task,
                        &normalized_session_id,
                    ) || task.result_items.iter().any(|item| {
                        result_item_uses_hls_session(item, &normalized_session_id)
                            && result_item_has_online_playback_after_cache_fill_failure(item)
                    }))
            })
    }

    pub fn is_playback_result_session_playable(
        &self,
        session_id: &str,
        completed_cache_playback_supported: bool,
    ) -> bool {
        let normalized_id = normalize(session_id);
        if normalized_id.is_empty() {
            return false;
        }
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner.visible_tasks_by_id.values().any(|task| {
            task.kind() == TaskKind::BilibiliProgressivePlayback
                && match task.state() {
                    TaskState::Playable => true,
                    TaskState::Completed => completed_cache_playback_supported,
                    _ => false,
                }
                && task.result_items.iter().any(|item| {
                    item.id == normalized_id
                        && result_item_can_serve_online_playback_after_task_completion(item)
                        && item.playback_session.is_some()
                })
        })
    }

    pub fn playback_hls_session_ids(&self, id: &str) -> Vec<String> {
        let Ok(normalized_id) = normalize_required_id(id) else {
            return Vec::new();
        };
        let inner = self.inner.lock().expect("task registry lock poisoned");
        let Some(task) = inner.visible_tasks_by_id.get(&normalized_id) else {
            return Vec::new();
        };
        playback_hls_session_ids(task)
    }

    pub fn interrupted_planning_result_session_ids(&self) -> HashSet<String> {
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner
            .tasks_by_id
            .values()
            .filter(|task| {
                task.kind() == TaskKind::BilibiliProgressivePlayback
                    && matches!(
                        (task.state(), task.message.as_str()),
                        (
                            TaskState::Failed,
                            PREPARING_INTERRUPTED_AFTER_RESTART_MESSAGE
                        ) | (TaskState::Cancelled, CANCELLED_AFTER_RESTART_MESSAGE)
                    )
            })
            .flat_map(|task| task.result_items.iter())
            .map(|item| item.id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect()
    }

    pub fn protected_hls_cache_session_ids(&self) -> HashSet<String> {
        let inner = self.inner.lock().expect("task registry lock poisoned");
        inner
            .visible_tasks_by_id
            .values()
            .filter(|task| task.kind() == TaskKind::BilibiliProgressivePlayback)
            .flat_map(|task| match task.state() {
                TaskState::Completed => protected_completed_result_hls_session_ids(task),
                TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled => Vec::new(),
                _ => playback_hls_session_ids(task),
            })
            .collect()
    }

    pub fn subscribe(&self, ids: &[String]) -> Result<TaskSubscription, Status> {
        let mut watched_ids = HashSet::new();
        for id in ids {
            let normalized_id = id.trim().to_owned();
            if normalized_id.is_empty() {
                return Err(Status::invalid_argument("Task id filter cannot be empty."));
            }

            watched_ids.insert(normalized_id);
        }

        let (sender, receiver) = mpsc::channel(WATCHER_EVENT_BUFFER_CAPACITY);
        let watcher_id = Uuid::new_v4();
        let lagged = Arc::new(AtomicBool::new(false));
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        inner.watchers.insert(
            watcher_id,
            TaskWatcher {
                watched_ids: watched_ids.clone(),
                sender,
                lagged: Arc::clone(&lagged),
            },
        );
        let mut snapshots = inner
            .visible_tasks_by_id
            .values()
            .filter(|task| watched_ids.is_empty() || watched_ids.contains(&task.id))
            .cloned()
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.id.cmp(&right.id));
        drop(inner);

        Ok(TaskSubscription {
            inner: Arc::clone(&self.inner),
            watcher_id,
            lagged,
            snapshots,
            receiver,
        })
    }

    pub fn fail_unrestorable_playback_tasks(
        &self,
        restorable_playable_session_ids: &HashSet<String>,
        restorable_completed_session_ids: &HashSet<String>,
    ) -> Vec<String> {
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let mut changed_tasks = Vec::new();
        let mut changed_task_ids = Vec::new();
        for task in inner.tasks_by_id.values_mut() {
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                continue;
            }
            let is_restorable = match task.state() {
                TaskState::Playable => {
                    let primary_restorable =
                        primary_hls_session_id(task)
                            .as_ref()
                            .is_some_and(|session_id| {
                                restorable_playable_session_ids.contains(session_id)
                            });
                    if primary_restorable
                        && clear_unrestorable_result_playback_metadata(
                            &mut task.result_items,
                            restorable_playable_session_ids,
                            restorable_completed_session_ids,
                        )
                    {
                        let updated_at = current_timestamp();
                        task.progress = result_items_progress(&task.result_items);
                        task.message =
                            "Playable online; some Bilibili playback results expired after restart."
                                .to_owned();
                        task.updated_at = Some(updated_at);
                        changed_task_ids.push(task.id.clone());
                        changed_tasks.push(task.clone());
                    }
                    primary_restorable
                }
                TaskState::Completed => {
                    let primary_restorable =
                        primary_hls_session_id(task)
                            .as_ref()
                            .is_some_and(|session_id| {
                                restorable_completed_session_ids.contains(session_id)
                            });
                    if primary_restorable
                        && clear_unrestorable_result_playback_metadata(
                            &mut task.result_items,
                            restorable_playable_session_ids,
                            restorable_completed_session_ids,
                        )
                    {
                        let updated_at = current_timestamp();
                        task.progress = result_items_progress(&task.result_items);
                        task.message =
                            "Completed offline cache restored; some Bilibili playback results expired after restart."
                                .to_owned();
                        task.updated_at = Some(updated_at);
                        changed_task_ids.push(task.id.clone());
                        changed_tasks.push(task.clone());
                    }
                    primary_restorable
                }
                _ => true,
            };
            if is_restorable {
                continue;
            }

            let updated_at = current_timestamp();
            task.state = TaskState::Failed.into();
            task.message = PLAYABLE_EXPIRED_AFTER_RESTART_MESSAGE.to_owned();
            task.library_item_id.clear();
            task.playback_source = None;
            task.playback_session = None;
            clear_result_playback_metadata(
                &mut task.result_items,
                TaskState::Failed,
                PLAYABLE_EXPIRED_AFTER_RESTART_MESSAGE,
            );
            task.updated_at = Some(copy_timestamp(&updated_at));
            task.finished_at = Some(updated_at);
            changed_task_ids.push(task.id.clone());
            changed_tasks.push(task.clone());
        }
        if changed_tasks.is_empty() {
            return changed_task_ids;
        }

        self.persist_tasks_and_publish(inner, changed_tasks, false, None);
        changed_task_ids
    }

    fn complete_task(
        &self,
        id: &str,
        state: TaskState,
        message: String,
        library_item_id: Option<String>,
        progress: Option<f64>,
    ) -> Result<Task, Status> {
        debug_assert!(is_terminal(state));
        let normalized_id = normalize_required_id(id)?;
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if self.persistence.is_some() && !self.persistence_available() {
            let outcome = self.persist_and_publish_pending(inner, true, None);
            if !outcome.is_committed() {
                return Err(Status::unavailable(
                    "Pending task state could not be persisted before task completion.",
                ));
            }
            inner = self.inner.lock().expect("task registry lock poisoned");
        }
        let durability_required = self.persistence.is_some();
        let Some(current_task) = inner.tasks_by_id.get(&normalized_id) else {
            return Err(task_not_found());
        };
        if is_terminal(current_task.state()) {
            return Ok(inner
                .visible_tasks_by_id
                .get(&normalized_id)
                .unwrap_or(current_task)
                .clone());
        }
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };

            let effective_state = if task.state() == TaskState::CancelRequested {
                TaskState::Cancelled
            } else {
                state
            };
            let effective_message =
                if task.state() == TaskState::CancelRequested && state != TaskState::Cancelled {
                    CANCELLED_BY_REQUEST_MESSAGE.to_owned()
                } else {
                    message
                };

            task.state = effective_state.into();
            task.message = effective_message.clone();
            if effective_state == TaskState::Succeeded
                && let Some(library_item_id) = library_item_id
            {
                task.library_item_id = library_item_id;
            }
            if effective_state == TaskState::Succeeded
                && let Some(progress) = progress
                && progress.is_finite()
            {
                task.progress = progress.clamp(0.0, 1.0);
            }
            if task.kind() == TaskKind::BilibiliProgressivePlayback
                && matches!(effective_state, TaskState::Failed | TaskState::Cancelled)
            {
                task.library_item_id.clear();
                task.playback_source = None;
                task.playback_session = None;
                clear_result_playback_metadata(
                    &mut task.result_items,
                    effective_state,
                    &effective_message,
                );
            }
            let finished_at = current_timestamp();
            task.updated_at = Some(copy_timestamp(&finished_at));
            task.finished_at = Some(finished_at);

            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);

        Self::clear_active_task_locked(&mut inner, &terminal_task);
        let outcome = self.persist_task_and_publish(inner, task.clone(), durability_required, None);
        if durability_required && !outcome.is_committed() {
            return Err(Status::unavailable(
                "Task completion could not be persisted durably.",
            ));
        }
        Ok(task)
    }

    fn from_persisted_records(
        records: Vec<PersistedTaskRecord>,
        store: Option<TaskStateStore>,
        persistence_configured: bool,
        retention_policy: TaskRetentionPolicy,
        resource_root_path: Option<PathBuf>,
    ) -> io::Result<Self> {
        validate_unique_task_record_identities(&records)?;
        let mut inner = RegistryInner::default();
        for record in records {
            let Some((mut task, download_options, playback_options, mut output)) =
                restore_persisted_record(record)
            else {
                continue;
            };
            if output.reconcile_legacy_task(&task).is_err() {
                continue;
            }
            task.output_summary = Some(output.summary());

            let is_active_task = is_active(task.state());
            let task_id = task.id.clone();
            let active_download_key = (is_active_task && task.kind() == TaskKind::BilibiliDownload)
                .then(|| {
                    active_key_for_task(&task, download_options.as_ref(), playback_options.as_ref())
                });
            if active_download_key
                .as_ref()
                .is_some_and(|active_key| inner.active_task_ids_by_key.contains_key(active_key))
            {
                continue;
            }
            if let Some(active_key) = active_download_key {
                inner
                    .active_task_ids_by_key
                    .insert(active_key, task_id.clone());
                inner.queued_task_ids.push_back(task_id.clone());
                inner
                    .download_options_by_id
                    .insert(task_id.clone(), download_options);
            }
            if is_active_task && task.kind() == TaskKind::BilibiliProgressivePlayback {
                inner
                    .playback_options_by_id
                    .insert(task_id.clone(), playback_options);
            }
            inner.outputs_by_task_id.insert(task_id.clone(), output);
            inner.tasks_by_id.insert(task_id, task);
        }
        inner.visible_tasks_by_id = inner.tasks_by_id.clone();
        inner.visible_outputs_by_task_id = inner
            .outputs_by_task_id
            .iter()
            .map(|(task_id, output)| {
                (
                    task_id.clone(),
                    Arc::new(VisibleTaskOutput::new(output.clone())),
                )
            })
            .collect();

        let orphan_resource_scan_pending = resource_root_path.is_some();
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            mutation_lock: Mutex::new(()),
            queue_notify: Arc::new(Notify::new()),
            persistence: store.map(TaskStatePersistence::new),
            persistence_configured,
            retention_policy,
            resource_root_path,
            resource_cleanup_lock: Mutex::new(()),
            resource_storage_available: AtomicBool::new(!orphan_resource_scan_pending),
            orphan_resource_scan_pending: AtomicBool::new(orphan_resource_scan_pending),
        })
    }

    fn persist_current_state(&self) -> bool {
        let _mutation_guard = self.mutation_guard();
        let inner = self.inner.lock().expect("task registry lock poisoned");
        self.persist_and_publish_pending(inner, true, None)
            .is_durable()
    }

    fn retire_expired_task_resources(&self) -> bool {
        self.retire_expired_task_resources_except(&HashSet::new())
    }

    fn retire_expired_task_resources_except(
        &self,
        excluded_resource_ids: &HashSet<String>,
    ) -> bool {
        let now = current_timestamp();
        let _mutation_guard = self.mutation_guard();
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task_ids = inner
            .outputs_by_task_id
            .iter()
            .filter(|(_, output)| output.has_expired_resources_except(&now, excluded_resource_ids))
            .map(|(task_id, _)| task_id.clone())
            .collect::<Vec<_>>();
        if task_ids.is_empty() {
            return false;
        }
        let checkpoint = RegistryMutationCheckpoint::capture(&inner);
        let mut retired_resource_ids = Vec::new();
        let mut changed_tasks = Vec::new();
        for task_id in task_ids {
            let retired = inner
                .outputs_by_task_id
                .get_mut(&task_id)
                .map(|output| output.retire_expired_resources_except(&now, excluded_resource_ids))
                .unwrap_or_default();
            if retired.is_empty() {
                continue;
            }
            retired_resource_ids.extend(retired);
            let summary = inner
                .outputs_by_task_id
                .get(&task_id)
                .expect("updated task output must exist")
                .summary();
            if let Some(task) = inner.tasks_by_id.get_mut(&task_id) {
                task.output_summary = Some(summary);
                task.updated_at = Some(copy_timestamp(&now));
                changed_tasks.push(task.clone());
            }
        }
        if retired_resource_ids.is_empty() {
            return false;
        }
        inner
            .pending_resource_cleanup_ids
            .extend(retired_resource_ids);
        let durability_required = self.persistence.is_some();
        let checkpoint = durability_required.then_some(checkpoint);
        self.persist_tasks_and_publish(inner, changed_tasks, durability_required, checkpoint)
            .is_committed()
    }

    fn persistence_snapshot_locked(
        &self,
        inner: &mut RegistryInner,
    ) -> Result<Option<TaskPersistenceSnapshot>, crate::task_output::TaskOutputValidationError>
    {
        reconcile_all_task_outputs_locked(inner)?;
        if self.persistence.is_none() {
            return Ok(None);
        }
        let pruned_task_ids =
            terminal_task_ids_to_prune_locked(inner, &self.retention_policy, &current_timestamp());
        let mut resource_cleanup_ids = inner.pending_resource_cleanup_ids.clone();
        for task_id in &pruned_task_ids {
            if let Some(output) = inner.outputs_by_task_id.get(task_id) {
                resource_cleanup_ids.extend(
                    output
                        .resources
                        .iter()
                        .map(|resource| resource.resource.id.clone()),
                );
            }
        }
        inner.persistence_generation += 1;
        Ok(Some(TaskPersistenceSnapshot {
            generation: inner.persistence_generation,
            records: persisted_records_locked(inner)
                .into_iter()
                .filter(|record| !pruned_task_ids.contains(&record.task.id))
                .collect(),
            resource_cleanup_ids: resource_cleanup_ids.into_iter().collect(),
            pruned_task_ids,
        }))
    }

    fn persist_and_publish_pending(
        &self,
        inner: MutexGuard<'_, RegistryInner>,
        durability_required: bool,
        rollback_checkpoint: Option<RegistryMutationCheckpoint>,
    ) -> PersistenceCommitOutcome {
        self.persist_and_publish_pending_with_policy(
            inner,
            durability_required,
            rollback_checkpoint,
            false,
        )
    }

    fn persist_and_publish_pending_with_policy(
        &self,
        mut inner: MutexGuard<'_, RegistryInner>,
        durability_required: bool,
        rollback_checkpoint: Option<RegistryMutationCheckpoint>,
        require_durable_commit: bool,
    ) -> PersistenceCommitOutcome {
        let (snapshot, snapshot_validation_failed) =
            match self.persistence_snapshot_locked(&mut inner) {
                Ok(snapshot) => (snapshot, false),
                Err(error) => {
                    if let Some(persistence) = self.persistence.as_ref() {
                        persistence.mark_unavailable();
                    }
                    eprintln!("Rejected invalid legacy task output snapshot: {error}");
                    (None, true)
                }
            };
        Self::refresh_pending_publications_locked(&mut inner);
        drop(inner);

        let outcome = match (
            snapshot_validation_failed,
            snapshot.as_ref(),
            self.persistence.as_ref(),
        ) {
            (true, _, _) => PersistenceCommitOutcome::Rejected,
            (false, Some(snapshot), Some(persistence)) => persistence.save_snapshot(snapshot),
            (false, None, _) if !durability_required => PersistenceCommitOutcome::Volatile,
            _ => PersistenceCommitOutcome::Rejected,
        };
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let mut rejected_resources_need_cleanup = false;
        match outcome {
            PersistenceCommitOutcome::InstalledButNotDurable if require_durable_commit => {
                let checkpoint = rollback_checkpoint.expect(
                    "a destructive side effect must retain a rollback checkpoint until durable",
                );
                checkpoint.restore(&mut inner);
            }
            PersistenceCommitOutcome::Durable
            | PersistenceCommitOutcome::InstalledButNotDurable => {
                let snapshot = snapshot
                    .as_ref()
                    .expect("committed persistence outcome must have a snapshot");
                apply_pruned_tasks_locked(&mut inner, &snapshot.pruned_task_ids);
                inner
                    .pending_resource_cleanup_ids
                    .extend(snapshot.resource_cleanup_ids.iter().cloned());
                Self::install_visible_snapshot_locked(&mut inner, &snapshot.records);
                if outcome.is_durable() {
                    Self::mark_resource_cleanup_durable_locked(
                        &mut inner,
                        snapshot.resource_cleanup_ids.clone(),
                    );
                }
                Self::refresh_pending_publications_locked(&mut inner);
                Self::publish_pending_locked(&mut inner);
            }
            PersistenceCommitOutcome::Volatile => {
                Self::install_visible_live_state_locked(&mut inner);
                Self::refresh_pending_publications_locked(&mut inner);
                Self::publish_pending_locked(&mut inner);
            }
            PersistenceCommitOutcome::Rejected => {
                if let Some(checkpoint) = rollback_checkpoint {
                    let rejected_resource_candidates = self
                        .resource_root_path
                        .as_ref()
                        .map(|_| output_resource_ids_locked(&inner))
                        .unwrap_or_default();
                    checkpoint.restore(&mut inner);
                    if !rejected_resource_candidates.is_empty() {
                        let reserved_resource_ids = reserved_resource_ids_locked(&inner);
                        let orphaned_resource_ids = rejected_resource_candidates
                            .into_iter()
                            .filter(|resource_id| !reserved_resource_ids.contains(resource_id))
                            .collect::<Vec<_>>();
                        rejected_resources_need_cleanup = !orphaned_resource_ids.is_empty();
                        inner
                            .durable_resource_cleanup_ids
                            .extend(orphaned_resource_ids);
                    }
                }
            }
            PersistenceCommitOutcome::Superseded => {
                debug_assert!(false, "serialized registry mutations cannot be superseded");
            }
        }
        drop(inner);
        if outcome.is_durable() {
            if self
                .orphan_resource_scan_pending
                .load(AtomicOrdering::Acquire)
            {
                self.retry_pending_orphan_resource_cleanup();
            } else {
                self.cleanup_durable_resource_bodies();
            }
        } else if rejected_resources_need_cleanup {
            self.cleanup_durable_resource_bodies();
        }
        outcome
    }

    fn persist_task_and_publish(
        &self,
        mut inner: MutexGuard<'_, RegistryInner>,
        task: Task,
        durability_required: bool,
        rollback_checkpoint: Option<RegistryMutationCheckpoint>,
    ) -> PersistenceCommitOutcome {
        Self::stage_publication_locked(&mut inner, task);
        self.persist_and_publish_pending(inner, durability_required, rollback_checkpoint)
    }

    fn persist_task_before_destructive_side_effect(
        &self,
        mut inner: MutexGuard<'_, RegistryInner>,
        task: Task,
        durability_required: bool,
        rollback_checkpoint: Option<RegistryMutationCheckpoint>,
    ) -> PersistenceCommitOutcome {
        Self::stage_publication_locked(&mut inner, task);
        self.persist_and_publish_pending_with_policy(
            inner,
            durability_required,
            rollback_checkpoint,
            true,
        )
    }

    fn persist_tasks_and_publish(
        &self,
        mut inner: MutexGuard<'_, RegistryInner>,
        tasks: impl IntoIterator<Item = Task>,
        durability_required: bool,
        rollback_checkpoint: Option<RegistryMutationCheckpoint>,
    ) -> PersistenceCommitOutcome {
        for task in tasks {
            Self::stage_publication_locked(&mut inner, task);
        }
        self.persist_and_publish_pending(inner, durability_required, rollback_checkpoint)
    }

    fn mutation_guard(&self) -> MutexGuard<'_, ()> {
        self.mutation_lock
            .lock()
            .expect("task registry mutation lock poisoned")
    }

    fn register_staged_task_output_resources(
        &self,
        task_id: &str,
        resources: &[TaskResourceRecord],
        resource_ids: &HashSet<String>,
    ) -> Result<HashSet<String>, Status> {
        let _cleanup_guard = self
            .resource_cleanup_lock
            .lock()
            .expect("task resource cleanup lock poisoned");
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if !inner.tasks_by_id.contains_key(task_id) {
            return Err(task_not_found());
        }
        prune_expired_resource_snapshots_locked(&mut inner, Instant::now());
        validate_task_output_resource_claims_locked(
            &inner,
            task_id,
            resources,
            resource_ids,
            false,
        )?;
        let current_task_resource_ids = inner
            .outputs_by_task_id
            .get(task_id)
            .into_iter()
            .flat_map(|output| &output.resources)
            .map(|resource| resource.resource.id.as_str())
            .collect::<HashSet<_>>();
        let resource_body_creation_ids = resource_ids
            .iter()
            .filter(|resource_id| !current_task_resource_ids.contains(resource_id.as_str()))
            .cloned()
            .collect();
        for resource_id in resource_ids {
            let count = inner
                .staged_resource_owner_counts
                .entry(resource_id.clone())
                .or_default();
            *count = count
                .checked_add(1)
                .expect("staged task resource owner count must not overflow");
        }
        Ok(resource_body_creation_ids)
    }

    fn release_staged_task_output_resources(&self, resource_ids: &HashSet<String>) {
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        release_staged_resource_owners_locked(&mut inner, resource_ids);
    }

    fn reject_staged_task_output_resources(&self, resource_ids: HashSet<String>) {
        let cleanup_needed = {
            let mut inner = self.inner.lock().expect("task registry lock poisoned");
            release_staged_resource_owners_locked(&mut inner, &resource_ids);
            reserve_unowned_resource_cleanup_locked(&mut inner, resource_ids)
        };
        if cleanup_needed
            && !self
                .orphan_resource_scan_pending
                .load(AtomicOrdering::Acquire)
        {
            self.cleanup_durable_resource_bodies();
        }
    }

    fn install_visible_snapshot_locked(inner: &mut RegistryInner, records: &[PersistedTaskRecord]) {
        let previous_outputs = std::mem::take(&mut inner.visible_outputs_by_task_id);
        inner.visible_tasks_by_id = records
            .iter()
            .map(|record| (record.task.id.clone(), record.task.clone()))
            .collect();
        inner.visible_outputs_by_task_id = records
            .iter()
            .map(|record| {
                let output = shared_visible_task_output(
                    previous_outputs.get(&record.task.id),
                    &record.output,
                );
                (record.task.id.clone(), output)
            })
            .collect();
    }

    fn install_visible_live_state_locked(inner: &mut RegistryInner) {
        let previous_outputs = std::mem::take(&mut inner.visible_outputs_by_task_id);
        inner.visible_tasks_by_id = inner.tasks_by_id.clone();
        inner.visible_outputs_by_task_id = inner
            .outputs_by_task_id
            .iter()
            .map(|(task_id, output)| {
                (
                    task_id.clone(),
                    shared_visible_task_output(previous_outputs.get(task_id), output),
                )
            })
            .collect();
    }

    fn mark_resource_cleanup_durable_locked(
        inner: &mut RegistryInner,
        resource_cleanup_ids: Vec<String>,
    ) {
        for resource_id in resource_cleanup_ids {
            if inner.pending_resource_cleanup_ids.remove(&resource_id) {
                inner.durable_resource_cleanup_ids.insert(resource_id);
            }
        }
    }

    fn mark_resource_storage_for_revalidation(&self, resource_id: &str, error: &io::Error) {
        self.resource_storage_available
            .store(false, AtomicOrdering::Release);
        self.inner
            .lock()
            .expect("task registry lock poisoned")
            .resource_storage_revalidation_ids
            .insert(resource_id.to_owned());
        eprintln!(
            "Failed to open task resource {resource_id}; disabling task output v2 until secure storage revalidation succeeds: {error}"
        );
    }

    fn revalidate_pending_resource_storage_locked(&self, resource_root_path: &Path) -> bool {
        let pending = {
            let mut inner = self.inner.lock().expect("task registry lock poisoned");
            prune_expired_resource_snapshots_locked(&mut inner, Instant::now());
            let now = current_timestamp();
            inner
                .resource_storage_revalidation_ids
                .iter()
                .map(|resource_id| {
                    (
                        resource_id.clone(),
                        resource_body_owner_record_locked(&inner, resource_id, &now),
                    )
                })
                .collect::<Vec<_>>()
        };
        if pending.is_empty() {
            return true;
        }

        let mut resolved = Vec::new();
        let mut succeeded = true;
        for (resource_id, record) in pending {
            let Some(record) = record else {
                resolved.push(resource_id);
                continue;
            };
            match validate_task_resource_body(resource_root_path, &record) {
                Ok(()) => resolved.push(resource_id),
                Err(error) => {
                    succeeded = false;
                    eprintln!(
                        "Task resource storage revalidation failed for {resource_id}: {error}"
                    );
                }
            }
        }

        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        for resource_id in resolved {
            inner.resource_storage_revalidation_ids.remove(&resource_id);
        }
        succeeded && inner.resource_storage_revalidation_ids.is_empty()
    }

    fn cleanup_durable_resource_bodies(&self) -> bool {
        self.cleanup_durable_resource_bodies_with_predelete_hook(|| {})
    }

    fn cleanup_durable_resource_bodies_with_predelete_hook(
        &self,
        predelete_hook: impl FnOnce(),
    ) -> bool {
        let Some(resource_root_path) = self.resource_root_path.as_ref() else {
            return true;
        };
        let _cleanup_guard = self
            .resource_cleanup_lock
            .lock()
            .expect("task resource cleanup lock poisoned");
        let storage_revalidated =
            self.revalidate_pending_resource_storage_locked(resource_root_path);
        let candidates = {
            let mut inner = self.inner.lock().expect("task registry lock poisoned");
            prune_expired_resource_snapshots_locked(&mut inner, Instant::now());
            let now = current_timestamp();
            let resource_body_owner_ids = resource_body_owner_ids_locked(&inner, &now);
            inner
                .durable_resource_cleanup_ids
                .iter()
                .filter(|resource_id| !resource_body_owner_ids.contains(resource_id.as_str()))
                .cloned()
                .collect::<Vec<_>>()
        };
        if candidates.is_empty() {
            if storage_revalidated
                && !self
                    .orphan_resource_scan_pending
                    .load(AtomicOrdering::Acquire)
            {
                self.resource_storage_available
                    .store(true, AtomicOrdering::Release);
            } else {
                self.resource_storage_available
                    .store(false, AtomicOrdering::Release);
            }
            return storage_revalidated;
        }
        predelete_hook();

        let mut cleaned = Vec::new();
        let mut cleanup_succeeded = storage_revalidated;
        for resource_id in candidates {
            let relative_path = TaskResourceRecord::relative_path_for_id(&resource_id);
            let body_removed = match remove_file_no_follow(resource_root_path, &relative_path) {
                Ok(_) => true,
                Err(error) => {
                    self.resource_storage_available
                        .store(false, AtomicOrdering::Release);
                    eprintln!("Failed to remove retired task resource {resource_id}: {error}");
                    cleanup_succeeded = false;
                    false
                }
            };
            if !body_removed {
                continue;
            }

            let relative_directory = TaskResourceRecord::relative_directory_for_id(&resource_id);
            match remove_empty_directory_no_follow(resource_root_path, &relative_directory) {
                Ok(_) => cleaned.push(resource_id),
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    eprintln!(
                        "Retired task resource directory {resource_id} is not empty; keeping the id reserved for a later cleanup: {error}"
                    );
                }
                Err(error) => {
                    self.resource_storage_available
                        .store(false, AtomicOrdering::Release);
                    cleanup_succeeded = false;
                    eprintln!(
                        "Failed to remove retired task resource directory {resource_id}: {error}"
                    );
                }
            }
        }
        if !cleaned.is_empty() {
            let mut inner = self.inner.lock().expect("task registry lock poisoned");
            let now = current_timestamp();
            let resource_body_owner_ids = resource_body_owner_ids_locked(&inner, &now);
            let cleaned = cleaned
                .into_iter()
                .filter(|resource_id| !resource_body_owner_ids.contains(resource_id.as_str()))
                .collect::<Vec<_>>();
            for resource_id in cleaned {
                inner.durable_resource_cleanup_ids.remove(&resource_id);
            }
        }
        if cleanup_succeeded
            && !self
                .orphan_resource_scan_pending
                .load(AtomicOrdering::Acquire)
        {
            self.resource_storage_available
                .store(true, AtomicOrdering::Release);
        }
        cleanup_succeeded
    }

    fn retry_pending_orphan_resource_cleanup(&self) {
        if !self
            .orphan_resource_scan_pending
            .load(AtomicOrdering::Acquire)
        {
            return;
        }
        self.resource_storage_available
            .store(false, AtomicOrdering::Release);
        if self.cleanup_orphaned_resource_bodies_at_startup() {
            self.orphan_resource_scan_pending
                .store(false, AtomicOrdering::Release);
            self.resource_storage_available
                .store(true, AtomicOrdering::Release);
        }
    }

    fn cleanup_orphaned_resource_bodies_at_startup(&self) -> bool {
        let Some(resource_root_path) = self.resource_root_path.as_ref() else {
            return true;
        };
        let resource_ids = match list_optional_directory_names_no_follow_bounded(
            resource_root_path,
            ".tvos-net-player/resources",
            MAX_TASK_RESOURCE_DIRECTORY_NAMES,
        ) {
            Ok(Some(resource_ids)) => resource_ids,
            Ok(None) => return true,
            Err(error) => {
                self.resource_storage_available
                    .store(false, AtomicOrdering::Release);
                eprintln!("Failed to scan task resource storage during startup: {error}");
                return false;
            }
        };
        if let Some(resource_id) = resource_ids
            .iter()
            .find(|resource_id| !resource_id_is_canonical(resource_id))
        {
            self.resource_storage_available
                .store(false, AtomicOrdering::Release);
            eprintln!(
                "Task resource storage contains a noncanonical directory name; remove it before task output v2 can be enabled: {resource_id}"
            );
            return false;
        }
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let now = current_timestamp();
        let resource_body_owner_ids = resource_body_owner_ids_locked(&inner, &now);
        let orphaned_resource_ids = resource_ids
            .into_iter()
            .filter(|resource_id| !resource_body_owner_ids.contains(resource_id.as_str()))
            .collect::<Vec<_>>();
        for resource_id in orphaned_resource_ids {
            inner.durable_resource_cleanup_ids.insert(resource_id);
        }
        drop(inner);
        self.cleanup_durable_resource_bodies()
    }

    fn clear_active_task_locked(inner: &mut RegistryInner, task: &TerminalTask) {
        if inner
            .active_task_ids_by_key
            .get(&task.active_key)
            .is_some_and(|active_task_id| active_task_id == &task.id)
        {
            inner.active_task_ids_by_key.remove(&task.active_key);
        }
        inner.running_cancellations_by_id.remove(&task.id);
        inner.planning_cancellations_by_id.remove(&task.id);
        inner.download_options_by_id.remove(&task.id);
        inner.playback_options_by_id.remove(&task.id);
    }

    fn clear_active_duplicate_key_locked(inner: &mut RegistryInner, task: &Task) {
        let active_key = active_key_for_task(
            task,
            inner
                .download_options_by_id
                .get(&task.id)
                .and_then(Option::as_ref),
            inner
                .playback_options_by_id
                .get(&task.id)
                .and_then(Option::as_ref),
        );
        if inner
            .active_task_ids_by_key
            .get(&active_key)
            .is_some_and(|active_task_id| active_task_id == &task.id)
        {
            inner.active_task_ids_by_key.remove(&active_key);
        }
    }

    fn terminal_task_locked(inner: &RegistryInner, task: &Task) -> TerminalTask {
        TerminalTask {
            id: task.id.clone(),
            active_key: active_key_for_task(
                task,
                inner
                    .download_options_by_id
                    .get(&task.id)
                    .and_then(Option::as_ref),
                inner
                    .playback_options_by_id
                    .get(&task.id)
                    .and_then(Option::as_ref),
            ),
        }
    }

    fn publish_locked(inner: &mut RegistryInner, task: Task) {
        let mut inactive_watchers = Vec::new();
        for (watcher_id, watcher) in &inner.watchers {
            if !watcher.matches(&task) {
                continue;
            }

            match watcher.sender.try_send(task.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    watcher.lagged.store(true, AtomicOrdering::Relaxed);
                    inactive_watchers.push(*watcher_id);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    inactive_watchers.push(*watcher_id);
                }
            }
        }

        for watcher_id in inactive_watchers {
            inner.watchers.remove(&watcher_id);
        }
    }

    fn stage_publication_locked(inner: &mut RegistryInner, task: Task) {
        inner
            .pending_publications_by_id
            .insert(task.id.clone(), task);
    }

    fn refresh_pending_publications_locked(inner: &mut RegistryInner) {
        for (task_id, pending) in &mut inner.pending_publications_by_id {
            if let Some(task) = inner.tasks_by_id.get(task_id) {
                *pending = task.clone();
            }
        }
    }

    fn publish_pending_locked(inner: &mut RegistryInner) {
        let pending = std::mem::take(&mut inner.pending_publications_by_id);
        for (task_id, staged_task) in pending {
            let task = inner
                .visible_tasks_by_id
                .get(&task_id)
                .cloned()
                .unwrap_or(staged_task);
            Self::publish_locked(inner, task);
        }
    }

    fn publish_volatile_locked(inner: &mut RegistryInner, task: Task) {
        if inner.pending_publications_by_id.contains_key(&task.id)
            || !inner.visible_tasks_by_id.contains_key(&task.id)
        {
            Self::stage_publication_locked(inner, task);
        } else {
            inner
                .visible_tasks_by_id
                .insert(task.id.clone(), task.clone());
            Self::publish_locked(inner, task);
        }
    }
}

impl Default for BilibiliTaskRegistry {
    fn default() -> Self {
        Self::from_persisted_records(
            Vec::new(),
            None,
            false,
            TaskRetentionPolicy::default(),
            None,
        )
        .expect("empty persisted task records should be valid")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRetentionPolicy {
    pub max_terminal_tasks: Option<usize>,
    pub max_terminal_task_age: Option<Duration>,
}

impl TaskRetentionPolicy {
    pub fn new(max_terminal_tasks: Option<usize>, max_terminal_task_age: Option<Duration>) -> Self {
        Self {
            max_terminal_tasks,
            max_terminal_task_age,
        }
    }
}

impl Default for TaskRetentionPolicy {
    fn default() -> Self {
        Self {
            max_terminal_tasks: Some(DEFAULT_MAX_TERMINAL_TASKS),
            max_terminal_task_age: Some(DEFAULT_TERMINAL_TASK_RETENTION),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TaskOutputSnapshot {
    pub(crate) task_id: String,
    pub(crate) revision: u64,
    pub(crate) snapshot_id: String,
    pub(crate) resource_lease_id: String,
    pub(crate) resource_lease_expires_at: Instant,
    pub(crate) encoded_bytes: usize,
    pub(crate) output: Arc<VisibleTaskOutput>,
}

pub(crate) struct OpenedTaskResource {
    pub(crate) record: TaskResourceRecord,
    pub(crate) file: File,
    pub(crate) last_modified: SystemTime,
    pub(crate) size_bytes: u64,
}

#[cfg(test)]
impl TaskOutputSnapshot {
    pub(crate) fn for_tests(
        task_id: impl Into<String>,
        revision: u64,
        snapshot_id: impl Into<String>,
        resource_lease_id: impl Into<String>,
        resource_lease_expires_at: Instant,
        results: Vec<TaskResult>,
        encoded_bytes: usize,
    ) -> Self {
        let snapshot_id = snapshot_id.into();
        let output = TaskOutputRecord {
            revision,
            snapshot_id: snapshot_id.clone(),
            primary_result_id: results
                .first()
                .map(|result| result.id.clone())
                .unwrap_or_default(),
            results,
            resources: Vec::new(),
            legacy_managed: false,
        };
        let mut output = VisibleTaskOutput::new(output);
        output.encoded_bytes = encoded_bytes;
        Self {
            task_id: task_id.into(),
            revision,
            snapshot_id,
            resource_lease_id: resource_lease_id.into(),
            resource_lease_expires_at,
            encoded_bytes,
            output: Arc::new(output),
        }
    }
}

#[derive(Debug)]
pub(crate) struct VisibleTaskOutput {
    pub(crate) record: TaskOutputRecord,
    encoded_bytes: usize,
    available_resources_by_id: HashMap<String, TaskResourceRecord>,
}

impl VisibleTaskOutput {
    fn new(record: TaskOutputRecord) -> Self {
        let available_ids = record
            .results
            .iter()
            .flat_map(|result| &result.artifacts)
            .filter(|artifact| artifact.state() == TaskArtifactState::Available)
            .filter_map(|artifact| artifact.resource.as_ref())
            .map(|resource| resource.id.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let available_resources_by_id = record
            .resources
            .iter()
            .filter(|resource| available_ids.contains(&resource.resource.id))
            .map(|resource| (resource.resource.id.clone(), resource.clone()))
            .collect();
        let encoded_bytes = record.encoded_bytes();
        Self {
            record,
            encoded_bytes,
            available_resources_by_id,
        }
    }
}

pub struct BilibiliTaskWorkItem {
    pub task_id: String,
    pub source: String,
    pub options: Option<BilibiliDownloadOptions>,
    pub cancellation: BilibiliTaskCancellation,
}

pub struct BilibiliPlaybackTaskCreation {
    pub task: Task,
    pub created: bool,
    pub cancellation: Option<BilibiliTaskCancellation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsSessionPublicationState {
    Published,
    Pending,
    Absent,
}

pub(crate) struct TaskCancellationOutcome {
    pub(crate) task: Task,
    pub(crate) hls_session_ids: Vec<String>,
}

#[derive(Clone, Default)]
pub struct BilibiliTaskCancellation {
    cancelled: Arc<AtomicBool>,
}

impl BilibiliTaskCancellation {
    pub fn is_cancel_requested(&self) -> bool {
        self.cancelled.load(AtomicOrdering::Relaxed)
    }

    fn request_cancel(&self) {
        self.cancelled.store(true, AtomicOrdering::Relaxed);
    }
}

#[derive(Default)]
pub struct BilibiliTaskProgress {
    pub progress: Option<f64>,
    pub downloaded_bytes: Option<i64>,
    pub total_bytes: Option<i64>,
    pub message: Option<String>,
}

pub struct TaskSubscription {
    inner: Arc<Mutex<RegistryInner>>,
    watcher_id: Uuid,
    lagged: Arc<AtomicBool>,
    snapshots: Vec<Task>,
    receiver: mpsc::Receiver<Task>,
}

impl TaskSubscription {
    pub fn snapshots(&self) -> &[Task] {
        &self.snapshots
    }

    pub async fn recv(&mut self) -> Result<Task, Status> {
        match self.receiver.recv().await {
            Some(task) => Ok(task),
            None if self.lagged.load(AtomicOrdering::Relaxed) => {
                Err(Status::resource_exhausted("Task watcher fell behind."))
            }
            None => Err(Status::unavailable("Task watcher closed.")),
        }
    }
}

impl Drop for TaskSubscription {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.watchers.remove(&self.watcher_id);
        }
    }
}

#[derive(Default)]
struct RegistryInner {
    tasks_by_id: HashMap<String, Task>,
    outputs_by_task_id: HashMap<String, TaskOutputRecord>,
    visible_tasks_by_id: HashMap<String, Task>,
    visible_outputs_by_task_id: HashMap<String, Arc<VisibleTaskOutput>>,
    download_options_by_id: HashMap<String, Option<BilibiliDownloadOptions>>,
    playback_options_by_id: HashMap<String, Option<BilibiliPlaybackOptions>>,
    active_task_ids_by_key: HashMap<ActiveBilibiliTaskKey, String>,
    queued_task_ids: VecDeque<String>,
    running_cancellations_by_id: HashMap<String, BilibiliTaskCancellation>,
    planning_cancellations_by_id: HashMap<String, BilibiliTaskCancellation>,
    watchers: HashMap<Uuid, TaskWatcher>,
    retained_resource_snapshots: HashMap<String, RetainedTaskResourceSnapshot>,
    staged_resource_owner_counts: HashMap<String, usize>,
    pending_resource_cleanup_ids: HashSet<String>,
    durable_resource_cleanup_ids: HashSet<String>,
    resource_storage_revalidation_ids: HashSet<String>,
    pending_publications_by_id: HashMap<String, Task>,
    persistence_generation: u64,
}

#[derive(Clone)]
struct RegistryMutationCheckpoint {
    tasks_by_id: HashMap<String, Task>,
    outputs_by_task_id: HashMap<String, TaskOutputRecord>,
    download_options_by_id: HashMap<String, Option<BilibiliDownloadOptions>>,
    playback_options_by_id: HashMap<String, Option<BilibiliPlaybackOptions>>,
    active_task_ids_by_key: HashMap<ActiveBilibiliTaskKey, String>,
    queued_task_ids: VecDeque<String>,
    running_cancellations_by_id: HashMap<String, BilibiliTaskCancellation>,
    planning_cancellations_by_id: HashMap<String, BilibiliTaskCancellation>,
    pending_resource_cleanup_ids: HashSet<String>,
    pending_publications_by_id: HashMap<String, Task>,
}

impl RegistryMutationCheckpoint {
    fn capture(inner: &RegistryInner) -> Self {
        Self {
            tasks_by_id: inner.tasks_by_id.clone(),
            outputs_by_task_id: inner.outputs_by_task_id.clone(),
            download_options_by_id: inner.download_options_by_id.clone(),
            playback_options_by_id: inner.playback_options_by_id.clone(),
            active_task_ids_by_key: inner.active_task_ids_by_key.clone(),
            queued_task_ids: inner.queued_task_ids.clone(),
            running_cancellations_by_id: inner.running_cancellations_by_id.clone(),
            planning_cancellations_by_id: inner.planning_cancellations_by_id.clone(),
            pending_resource_cleanup_ids: inner.pending_resource_cleanup_ids.clone(),
            pending_publications_by_id: inner.pending_publications_by_id.clone(),
        }
    }

    fn restore(self, inner: &mut RegistryInner) {
        inner.tasks_by_id = self.tasks_by_id;
        inner.outputs_by_task_id = self.outputs_by_task_id;
        inner.download_options_by_id = self.download_options_by_id;
        inner.playback_options_by_id = self.playback_options_by_id;
        inner.active_task_ids_by_key = self.active_task_ids_by_key;
        inner.queued_task_ids = self.queued_task_ids;
        inner.running_cancellations_by_id = self.running_cancellations_by_id;
        inner.planning_cancellations_by_id = self.planning_cancellations_by_id;
        inner.pending_resource_cleanup_ids = self.pending_resource_cleanup_ids;
        inner.pending_publications_by_id = self.pending_publications_by_id;
    }
}

struct RetainedTaskResourceSnapshot {
    expires_at: Instant,
    output: Arc<VisibleTaskOutput>,
}

impl RetainedTaskResourceSnapshot {
    fn from_output(snapshot: &TaskOutputSnapshot) -> Self {
        Self {
            expires_at: snapshot.resource_lease_expires_at,
            output: Arc::clone(&snapshot.output),
        }
    }
}

struct TaskStatePersistence {
    store: TaskStateStore,
    state: Mutex<TaskStatePersistenceState>,
    available: AtomicBool,
}

impl TaskStatePersistence {
    fn new(store: TaskStateStore) -> Self {
        Self {
            store,
            state: Mutex::new(TaskStatePersistenceState::default()),
            available: AtomicBool::new(true),
        }
    }

    fn is_available(&self) -> bool {
        self.available.load(AtomicOrdering::Acquire)
    }

    fn mark_unavailable(&self) {
        self.available.store(false, AtomicOrdering::Release);
    }

    fn path(&self) -> &Path {
        self.store.path()
    }

    fn save_snapshot(&self, snapshot: &TaskPersistenceSnapshot) -> PersistenceCommitOutcome {
        let mut state = self.state.lock().expect("task persistence lock poisoned");
        if snapshot.generation < state.latest_seen_generation {
            return PersistenceCommitOutcome::Superseded;
        }
        state.latest_seen_generation = snapshot.generation;

        match self.store.save(&snapshot.records) {
            Ok(TaskStateSaveOutcome::Durable) => {
                self.available.store(true, AtomicOrdering::Release);
                PersistenceCommitOutcome::Durable
            }
            Ok(TaskStateSaveOutcome::InstalledButNotDurable(error)) => {
                self.available.store(false, AtomicOrdering::Release);
                eprintln!(
                    "Installed Bilibili task state at {}, but directory synchronization failed; task output v2 remains unavailable until a durable retry: {error}",
                    self.path().display()
                );
                PersistenceCommitOutcome::InstalledButNotDurable
            }
            Err(error) => {
                self.available.store(false, AtomicOrdering::Release);
                eprintln!(
                    "Failed to persist Bilibili task state to {}: {error}",
                    self.path().display()
                );
                PersistenceCommitOutcome::Rejected
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistenceCommitOutcome {
    Durable,
    InstalledButNotDurable,
    Volatile,
    Rejected,
    Superseded,
}

impl PersistenceCommitOutcome {
    fn is_committed(self) -> bool {
        matches!(
            self,
            Self::Durable | Self::InstalledButNotDurable | Self::Volatile
        )
    }

    fn is_durable(self) -> bool {
        self == Self::Durable
    }
}

#[derive(Default)]
struct TaskStatePersistenceState {
    latest_seen_generation: u64,
}

struct TaskPersistenceSnapshot {
    generation: u64,
    records: Vec<PersistedTaskRecord>,
    resource_cleanup_ids: Vec<String>,
    pruned_task_ids: Vec<String>,
}

struct TerminalTask {
    id: String,
    active_key: ActiveBilibiliTaskKey,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ActiveBilibiliTaskKey {
    kind: i32,
    source: String,
    quality_preference: String,
    encoding_preference: String,
    audio_language: String,
    prefer_tv_api: bool,
    download_subtitles: bool,
    download_danmaku: bool,
    subtitle_ai_policy: i32,
    download_cover: bool,
    danmaku_formats: Vec<i32>,
}

impl ActiveBilibiliTaskKey {
    fn download(source: &str, options: Option<&BilibiliDownloadOptions>) -> Self {
        let mut key = Self::new(TaskKind::BilibiliDownload, source);
        if let Some(options) = options {
            key.quality_preference = normalize_option_string(&options.quality_preference);
            key.encoding_preference = normalize_option_string(&options.encoding_preference);
            key.audio_language = normalize_option_string(&options.audio_language);
            key.prefer_tv_api = options.prefer_tv_api;
            key.download_subtitles = options.download_subtitles;
            key.download_danmaku = options.download_danmaku;
            key.subtitle_ai_policy = normalize_subtitle_ai_policy_key(options.subtitle_ai_policy);
            key.download_cover = options.download_cover;
            key.danmaku_formats = normalize_danmaku_format_keys(&options.danmaku_formats);
        }
        key
    }

    fn playback(source: &str, options: Option<&BilibiliPlaybackOptions>) -> Self {
        let mut key = Self::new(TaskKind::BilibiliProgressivePlayback, source);
        if let Some(options) = options {
            key.quality_preference = normalize_option_string(&options.quality_preference);
            key.encoding_preference = normalize_option_string(&options.encoding_preference);
            key.audio_language = normalize_option_string(&options.audio_language);
            key.prefer_tv_api = options.prefer_tv_api;
        }
        key
    }

    fn new(kind: TaskKind, source: &str) -> Self {
        Self {
            kind: kind.into(),
            source: normalize(source),
            quality_preference: String::new(),
            encoding_preference: String::new(),
            audio_language: String::new(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
        }
    }
}

struct TaskWatcher {
    watched_ids: HashSet<String>,
    sender: mpsc::Sender<Task>,
    lagged: Arc<AtomicBool>,
}

impl TaskWatcher {
    fn matches(&self, task: &Task) -> bool {
        self.watched_ids.is_empty() || self.watched_ids.contains(&task.id)
    }
}

fn normalize(value: &str) -> String {
    value.trim().to_owned()
}

fn normalize_option_string(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_subtitle_ai_policy_key(value: i32) -> i32 {
    match BilibiliSubtitleAiPolicy::try_from(value) {
        Ok(BilibiliSubtitleAiPolicy::Unspecified | BilibiliSubtitleAiPolicy::Include) => {
            BilibiliSubtitleAiPolicy::Unspecified.into()
        }
        Ok(policy) => policy.into(),
        Err(_) => value,
    }
}

fn normalize_danmaku_format_keys(values: &[i32]) -> Vec<i32> {
    let mut normalized = values
        .iter()
        .copied()
        .filter(|value| {
            !matches!(
                BilibiliDanmakuFormat::try_from(*value),
                Ok(BilibiliDanmakuFormat::Unspecified)
            )
        })
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn normalize_required_id(id: &str) -> Result<String, Status> {
    let normalized_id = normalize(id);
    if normalized_id.is_empty() {
        return Err(Status::invalid_argument("Task id is required."));
    }

    Ok(normalized_id)
}

fn task_not_found() -> Status {
    Status::not_found("Task not found.")
}

fn result_items_progress(items: &[BilibiliTaskResultItem]) -> f64 {
    if items.is_empty() {
        return 0.0;
    }

    let finished = items
        .iter()
        .filter(|item| {
            matches!(
                TaskState::try_from(item.state).unwrap_or(TaskState::Unspecified),
                TaskState::Playable
                    | TaskState::Completed
                    | TaskState::Failed
                    | TaskState::Cancelled
            )
        })
        .count();
    (finished as f64 / items.len() as f64).clamp(0.0, 1.0)
}

fn refresh_result_item_playback_source(
    task: &mut Task,
    session_id: &str,
    playback_source: &PlaybackSource,
) {
    for item in &mut task.result_items {
        if item.id == session_id
            && result_item_can_serve_online_playback_after_task_completion(item)
        {
            item.playback_source = Some(playback_source.clone());
        }
    }
}

fn refresh_result_item_playback_session(
    task: &mut Task,
    session_id: &str,
    playback_session: &BilibiliPlaybackSession,
) {
    for item in &mut task.result_items {
        if result_item_can_serve_online_playback_after_task_completion(item)
            && result_item_uses_hls_session(item, session_id)
        {
            item.playback_session = Some(playback_session.clone());
        }
    }
}

fn primary_playback_source_for_refresh(
    task: &Task,
    playback_source: &PlaybackSource,
) -> PlaybackSource {
    let mut refreshed = playback_source.clone();
    refreshed.item_id = if task.state() == TaskState::Completed && !task.library_item_id.is_empty()
    {
        task.library_item_id.clone()
    } else {
        task.id.clone()
    };
    refreshed
}

fn task_uses_hls_session_as_primary(task: &Task, session_id: &str) -> bool {
    primary_hls_session_id(task).is_some_and(|primary_id| primary_id == session_id)
}

fn task_uses_hls_session(task: &Task, session_id: &str) -> bool {
    task_uses_hls_session_as_primary(task, session_id)
        || task
            .result_items
            .iter()
            .any(|item| result_item_uses_hls_session(item, session_id))
}

fn task_has_playable_or_completed_result_session(task: &Task, session_id: &str) -> bool {
    task.result_items.iter().any(|item| {
        result_item_uses_hls_session(item, session_id)
            && result_item_state(item)
                .is_some_and(|state| matches!(state, TaskState::Playable | TaskState::Completed))
    })
}

fn completed_task_has_playable_result_session(task: &Task, session_id: &str) -> bool {
    task.state() == TaskState::Completed
        && task.result_items.iter().any(|item| {
            result_item_uses_hls_session(item, session_id)
                && (result_item_state(item) == Some(TaskState::Playable)
                    || result_item_has_online_playback_after_cache_fill_failure(item))
        })
}

fn result_item_has_online_playback_after_cache_fill_failure(item: &BilibiliTaskResultItem) -> bool {
    result_item_state(item) == Some(TaskState::Failed)
        && item.message.contains("offline cache fill failed")
        && item.playback_source.is_some()
        && item.playback_session.is_some()
}

fn task_has_online_playback_after_cache_fill_failure(task: &Task, session_id: &str) -> bool {
    task.state() == TaskState::Playable
        && task.message.contains("offline cache fill failed")
        && task.playback_source.is_some()
        && task.playback_session.is_some()
        && task_uses_hls_session_as_primary(task, session_id)
}

fn result_item_can_serve_online_playback_after_task_completion(
    item: &BilibiliTaskResultItem,
) -> bool {
    result_item_state(item)
        .is_some_and(|state| matches!(state, TaskState::Playable | TaskState::Completed))
        || result_item_has_online_playback_after_cache_fill_failure(item)
}

fn result_item_can_survive_restore_when_session_is_restorable(
    item: &BilibiliTaskResultItem,
    restorable_playable_session_ids: &HashSet<String>,
    restorable_completed_session_ids: &HashSet<String>,
) -> bool {
    let session_ids = result_item_hls_session_ids(item);
    match result_item_state(item) {
        Some(TaskState::Completed) => session_ids.iter().any(|session_id| {
            restorable_completed_session_ids.contains(session_id)
                && item.library_item_id == HlsCacheStore::completed_library_item_id(session_id)
        }),
        Some(TaskState::Playable) => session_ids
            .iter()
            .any(|session_id| restorable_playable_session_ids.contains(session_id)),
        Some(TaskState::Failed)
            if result_item_has_online_playback_after_cache_fill_failure(item) =>
        {
            session_ids
                .iter()
                .any(|session_id| restorable_playable_session_ids.contains(session_id))
        }
        _ => false,
    }
}

fn primary_hls_session_id(task: &Task) -> Option<String> {
    task.playback_session
        .as_ref()
        .map(|session| session.id.clone())
        .or_else(|| {
            task.playback_source
                .as_ref()
                .map(|source| source.item_id.clone())
        })
}

fn playback_source_uri_for_session(task: &Task, session_id: &str) -> Option<String> {
    task.playback_source
        .as_ref()
        .filter(|source| {
            source.item_id == session_id
                || task.id == session_id
                || task_uses_hls_session_as_primary(task, session_id)
        })
        .map(|source| source.uri.clone())
        .or_else(|| {
            task.result_items
                .iter()
                .find(|item| item.id == session_id)
                .and_then(|item| item.playback_source.as_ref())
                .map(|source| source.uri.clone())
        })
}

fn result_item_state(item: &BilibiliTaskResultItem) -> Option<TaskState> {
    TaskState::try_from(item.state).ok()
}

fn clear_result_playback_metadata(
    items: &mut [BilibiliTaskResultItem],
    terminal_state: TaskState,
    terminal_message: &str,
) {
    for item in items {
        if !matches!(
            result_item_state(item).unwrap_or(TaskState::Unspecified),
            TaskState::Failed | TaskState::Cancelled
        ) {
            item.state = terminal_state.into();
            item.message = terminal_message.to_owned();
        }
        item.library_item_id.clear();
        item.playback_source = None;
        item.playback_session = None;
    }
}

fn clear_progressive_playback_runtime_metadata(
    task: &mut Task,
    terminal_state: TaskState,
    terminal_message: &str,
) {
    task.library_item_id.clear();
    task.playback_source = None;
    task.playback_session = None;
    clear_result_playback_metadata(&mut task.result_items, terminal_state, terminal_message);
}

fn clear_unrestorable_result_playback_metadata(
    items: &mut [BilibiliTaskResultItem],
    restorable_playable_session_ids: &HashSet<String>,
    restorable_completed_session_ids: &HashSet<String>,
) -> bool {
    let mut changed = false;
    for item in items {
        if result_item_can_survive_restore_when_session_is_restorable(
            item,
            restorable_playable_session_ids,
            restorable_completed_session_ids,
        ) {
            continue;
        }
        if !result_item_can_serve_online_playback_after_task_completion(item) {
            continue;
        }
        item.state = TaskState::Failed.into();
        item.message = PLAYABLE_EXPIRED_AFTER_RESTART_MESSAGE.to_owned();
        item.library_item_id.clear();
        item.playback_source = None;
        item.playback_session = None;
        changed = true;
    }
    changed
}

fn clear_unrestorable_result_playback_metadata_for_session(
    items: &mut [BilibiliTaskResultItem],
    session_id: &str,
    message: &str,
) -> bool {
    let mut changed = false;
    for item in items {
        if !result_item_can_serve_online_playback_after_task_completion(item)
            || !result_item_uses_hls_session(item, session_id)
        {
            continue;
        }
        item.state = TaskState::Failed.into();
        item.message = message.to_owned();
        item.library_item_id.clear();
        item.playback_source = None;
        item.playback_session = None;
        changed = true;
    }
    changed
}

fn mark_result_cache_fill_failed_for_session(
    items: &mut [BilibiliTaskResultItem],
    session_id: &str,
    message: &str,
) -> bool {
    let mut changed = false;
    for item in items {
        if !result_item_state(item)
            .is_some_and(|state| matches!(state, TaskState::Playable | TaskState::Completed))
            || !result_item_uses_hls_session(item, session_id)
        {
            continue;
        }
        item.state = TaskState::Failed.into();
        item.message = message.to_owned();
        item.library_item_id.clear();
        changed = true;
    }
    changed
}

fn clear_completed_result_cache_item_for_session(
    items: &mut [BilibiliTaskResultItem],
    session_id: &str,
    library_item_id: &str,
    message: &str,
) -> bool {
    let mut changed = false;
    for item in items {
        if item.library_item_id != library_item_id
            || result_item_state(item) != Some(TaskState::Completed)
            || !result_item_uses_hls_session(item, session_id)
        {
            continue;
        }
        item.state = TaskState::Failed.into();
        item.message = message.to_owned();
        item.library_item_id.clear();
        item.playback_source = None;
        item.playback_session = None;
        changed = true;
    }
    changed
}

fn playback_hls_session_ids(task: &Task) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(session_id) = primary_hls_session_id(task) {
        ids.push(session_id);
    }
    ids.extend(
        task.result_items
            .iter()
            .flat_map(result_item_hls_session_ids),
    );
    ids.sort();
    ids.dedup();
    ids
}

fn task_has_playable_hls_session(task: &Task, session_id: &str) -> bool {
    task.kind() == TaskKind::BilibiliProgressivePlayback
        && ((task.state() == TaskState::Playable && task_uses_hls_session(task, session_id))
            || completed_task_has_playable_result_session(task, session_id))
}

fn protected_completed_result_hls_session_ids(task: &Task) -> Vec<String> {
    let primary_session_id = primary_hls_session_id(task);
    let mut ids = task
        .result_items
        .iter()
        .filter(|item| {
            matches!(
                result_item_state(item).unwrap_or(TaskState::Unspecified),
                TaskState::Playable | TaskState::Completed
            )
        })
        .flat_map(result_item_hls_session_ids)
        .filter(|session_id| primary_session_id.as_ref() != Some(session_id))
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids
}

fn result_item_uses_hls_session(item: &BilibiliTaskResultItem, session_id: &str) -> bool {
    result_item_hls_session_ids(item)
        .iter()
        .any(|item_session_id| item_session_id == session_id)
}

fn result_item_hls_session_ids(item: &BilibiliTaskResultItem) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(session) = item.playback_session.as_ref() {
        push_hls_session_identity(&mut ids, &session.id);
    }
    if let Some(source) = item.playback_source.as_ref() {
        push_hls_session_identity(&mut ids, &source.item_id);
    }
    if item.playback_source.is_some() || item.playback_session.is_some() {
        push_hls_session_identity(&mut ids, &item.id);
    }
    ids.sort();
    ids.dedup();
    ids
}

fn push_hls_session_identity(ids: &mut Vec<String>, id: &str) {
    if id.is_empty() {
        return;
    }
    ids.push(HlsCacheStore::session_id_from_library_item_id(id).unwrap_or_else(|| id.to_owned()));
}

fn completed_playback_task_id_for_hls_session_locked(
    tasks_by_id: &HashMap<String, Task>,
    session_id: &str,
) -> Option<String> {
    tasks_by_id.values().find_map(|task| {
        (task.kind() == TaskKind::BilibiliProgressivePlayback
            && task.state() == TaskState::Completed
            && task_uses_hls_session_as_primary(task, session_id))
        .then(|| task.id.clone())
    })
}

fn completed_playback_task_id_for_any_hls_session_locked(
    tasks_by_id: &HashMap<String, Task>,
    session_id: &str,
) -> Option<String> {
    tasks_by_id.values().find_map(|task| {
        (task.kind() == TaskKind::BilibiliProgressivePlayback
            && task.state() == TaskState::Completed
            && playback_hls_session_ids(task)
                .iter()
                .any(|task_session_id| task_session_id == session_id))
        .then(|| task.id.clone())
    })
}

fn playback_task_id_for_completed_result_cache_item_locked(
    tasks_by_id: &HashMap<String, Task>,
    session_id: &str,
    library_item_id: &str,
) -> Option<String> {
    tasks_by_id.values().find_map(|task| {
        (task.kind() == TaskKind::BilibiliProgressivePlayback
            && task.state() == TaskState::Playable
            && task.result_items.iter().any(|item| {
                item.library_item_id == library_item_id
                    && result_item_uses_hls_session(item, session_id)
                    && result_item_state(item) == Some(TaskState::Completed)
            }))
        .then(|| task.id.clone())
    })
}

fn completed_playback_task_matches_hls_cache_item(
    task: &Task,
    session_id: &str,
    library_item_id: &str,
) -> bool {
    if task.kind() != TaskKind::BilibiliProgressivePlayback || task.state() != TaskState::Completed
    {
        return false;
    }
    playback_task_has_completed_hls_cache_item(task, session_id, library_item_id)
}

fn playback_task_has_completed_hls_cache_item(
    task: &Task,
    session_id: &str,
    library_item_id: &str,
) -> bool {
    if task.kind() != TaskKind::BilibiliProgressivePlayback {
        return false;
    }
    if task.library_item_id == library_item_id && task_uses_hls_session_as_primary(task, session_id)
    {
        return true;
    }
    task.result_items.iter().any(|item| {
        result_item_state(item) == Some(TaskState::Completed)
            && item.library_item_id == library_item_id
            && result_item_uses_hls_session(item, session_id)
    })
}

fn active_key_for_task(
    task: &Task,
    download_options: Option<&BilibiliDownloadOptions>,
    playback_options: Option<&BilibiliPlaybackOptions>,
) -> ActiveBilibiliTaskKey {
    match task.kind() {
        TaskKind::BilibiliProgressivePlayback => {
            ActiveBilibiliTaskKey::playback(&task.source, playback_options)
        }
        _ => ActiveBilibiliTaskKey::download(&task.source, download_options),
    }
}

fn is_active(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Queued
            | TaskState::Running
            | TaskState::CancelRequested
            | TaskState::Planned
            | TaskState::Preparing
    )
}

fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled | TaskState::Completed
    )
}

fn restore_persisted_record(
    record: PersistedTaskRecord,
) -> Option<(
    Task,
    Option<BilibiliDownloadOptions>,
    Option<BilibiliPlaybackOptions>,
    TaskOutputRecord,
)> {
    let mut task = record.task;
    task.id = normalize(&task.id);
    task.source = normalize(&task.source);
    let task_kind = task.kind();
    let task_state = task.state();
    if task.id.is_empty()
        || task.source.is_empty()
        || !matches!(
            task_kind,
            TaskKind::BilibiliDownload | TaskKind::BilibiliProgressivePlayback
        )
        || !matches!(
            task_state,
            TaskState::Queued
                | TaskState::Running
                | TaskState::Succeeded
                | TaskState::Failed
                | TaskState::CancelRequested
                | TaskState::Cancelled
                | TaskState::Planned
                | TaskState::Preparing
                | TaskState::Playable
                | TaskState::Completed
        )
    {
        return None;
    }

    if task_kind == TaskKind::BilibiliProgressivePlayback {
        if task_state == TaskState::Planned && task.playback_session.is_none() {
            return None;
        }
        if task_state == TaskState::Playable
            && (task.playback_source.is_none() || task.playback_session.is_none())
        {
            return None;
        }
        if task_state == TaskState::Completed {
            if task.library_item_id.is_empty() {
                return None;
            }
            if let Some(playback_source) = task.playback_source.as_mut() {
                playback_source.item_id = task.library_item_id.clone();
                playback_source.expires_at = None;
            }
        }
    }

    if task_state == TaskState::CancelRequested {
        let updated_at = current_timestamp();
        task.state = TaskState::Cancelled.into();
        task.message = CANCELLED_AFTER_RESTART_MESSAGE.to_owned();
        if task_kind == TaskKind::BilibiliProgressivePlayback {
            clear_progressive_playback_runtime_metadata(
                &mut task,
                TaskState::Cancelled,
                CANCELLED_AFTER_RESTART_MESSAGE,
            );
        }
        task.updated_at = Some(copy_timestamp(&updated_at));
        task.finished_at = Some(updated_at);
    } else if task_kind == TaskKind::BilibiliDownload && task_state == TaskState::Running {
        task.state = TaskState::Queued.into();
        task.message = REQUEUED_AFTER_RESTART_MESSAGE.to_owned();
        task.updated_at = Some(current_timestamp());
        task.finished_at = None;
    } else if task_kind == TaskKind::BilibiliProgressivePlayback
        && matches!(task_state, TaskState::Preparing | TaskState::Running)
    {
        let updated_at = current_timestamp();
        task.state = TaskState::Failed.into();
        task.message = PREPARING_INTERRUPTED_AFTER_RESTART_MESSAGE.to_owned();
        clear_progressive_playback_runtime_metadata(
            &mut task,
            TaskState::Failed,
            PREPARING_INTERRUPTED_AFTER_RESTART_MESSAGE,
        );
        task.updated_at = Some(copy_timestamp(&updated_at));
        task.finished_at = Some(updated_at);
    }

    Some((task, record.options, record.playback_options, record.output))
}

fn reconcile_all_task_outputs_locked(
    inner: &mut RegistryInner,
) -> Result<(), crate::task_output::TaskOutputValidationError> {
    let task_ids = inner.tasks_by_id.keys().cloned().collect::<Vec<_>>();
    for task_id in task_ids {
        reconcile_task_output_locked(inner, &task_id)?;
    }
    Ok(())
}

fn prune_expired_resource_snapshots_locked(inner: &mut RegistryInner, now: Instant) {
    inner
        .retained_resource_snapshots
        .retain(|_, snapshot| snapshot.expires_at > now);
}

fn resource_body_owner_ids_locked<'a>(
    inner: &'a RegistryInner,
    now: &Timestamp,
) -> HashSet<&'a str> {
    inner
        .outputs_by_task_id
        .values()
        .flat_map(|output| &output.resources)
        .map(|resource| resource.resource.id.as_str())
        .chain(
            inner
                .visible_outputs_by_task_id
                .values()
                .flat_map(|output| &output.record.resources)
                .map(|resource| resource.resource.id.as_str()),
        )
        .chain(
            inner
                .retained_resource_snapshots
                .values()
                .flat_map(|snapshot| &snapshot.output.record.resources)
                .filter(|resource| {
                    resource
                        .resource
                        .expires_at
                        .as_ref()
                        .is_none_or(|expires_at| timestamp_nanos(expires_at) > timestamp_nanos(now))
                })
                .map(|resource| resource.resource.id.as_str()),
        )
        .chain(
            inner
                .staged_resource_owner_counts
                .keys()
                .map(String::as_str),
        )
        .collect()
}

fn resource_body_owner_record_locked(
    inner: &RegistryInner,
    resource_id: &str,
    now: &Timestamp,
) -> Option<TaskResourceRecord> {
    inner
        .outputs_by_task_id
        .values()
        .flat_map(|output| &output.resources)
        .find(|resource| resource.resource.id == resource_id)
        .cloned()
        .or_else(|| {
            inner
                .visible_outputs_by_task_id
                .values()
                .flat_map(|output| &output.record.resources)
                .find(|resource| resource.resource.id == resource_id)
                .cloned()
        })
        .or_else(|| {
            inner
                .retained_resource_snapshots
                .values()
                .flat_map(|snapshot| &snapshot.output.record.resources)
                .find(|resource| resource.resource.id == resource_id)
                .filter(|resource| {
                    resource
                        .resource
                        .expires_at
                        .as_ref()
                        .is_none_or(|expires_at| timestamp_nanos(expires_at) > timestamp_nanos(now))
                })
                .cloned()
        })
}

fn validate_task_resource_body(
    resource_root_path: &Path,
    record: &TaskResourceRecord,
) -> io::Result<()> {
    let file = open_read_no_follow(resource_root_path, &record.relative_path())?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task resource body is not a regular file",
        ));
    }
    if record.resource.size_known
        && u64::try_from(record.resource.size_bytes).ok() != Some(metadata.len())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task resource body size does not match its durable metadata",
        ));
    }
    Ok(())
}

fn output_resource_ids_locked(inner: &RegistryInner) -> HashSet<String> {
    inner
        .outputs_by_task_id
        .values()
        .flat_map(|output| &output.resources)
        .map(|resource| resource.resource.id.clone())
        .collect()
}

fn reserved_resource_ids_locked(inner: &RegistryInner) -> HashSet<String> {
    output_resource_ids_locked(inner)
        .into_iter()
        .chain(
            inner
                .visible_outputs_by_task_id
                .values()
                .flat_map(|output| output.available_resources_by_id.keys().cloned()),
        )
        .chain(
            inner
                .retained_resource_snapshots
                .values()
                .flat_map(|snapshot| &snapshot.output.record.resources)
                .map(|resource| resource.resource.id.clone()),
        )
        .chain(inner.pending_resource_cleanup_ids.iter().cloned())
        .chain(inner.durable_resource_cleanup_ids.iter().cloned())
        .chain(inner.resource_storage_revalidation_ids.iter().cloned())
        .chain(inner.staged_resource_owner_counts.keys().cloned())
        .collect()
}

fn validate_task_output_resource_claims_locked(
    inner: &RegistryInner,
    task_id: &str,
    resources: &[TaskResourceRecord],
    candidate_resource_ids: &HashSet<String>,
    claim_is_registered: bool,
) -> Result<(), Status> {
    let current_task_resources_by_id = inner
        .outputs_by_task_id
        .get(task_id)
        .into_iter()
        .flat_map(|output| &output.resources)
        .map(|resource| (resource.resource.id.as_str(), resource))
        .collect::<HashMap<_, _>>();
    let other_task_resource_ids = inner
        .outputs_by_task_id
        .iter()
        .filter(|(candidate_task_id, _)| candidate_task_id.as_str() != task_id)
        .flat_map(|(_, output)| &output.resources)
        .map(|resource| resource.resource.id.as_str())
        .collect::<HashSet<_>>();
    let retained_resource_ids = inner
        .retained_resource_snapshots
        .values()
        .flat_map(|snapshot| &snapshot.output.record.resources)
        .map(|resource| resource.resource.id.as_str())
        .collect::<HashSet<_>>();
    let visible_resource_ids = inner
        .visible_outputs_by_task_id
        .values()
        .flat_map(|output| output.available_resources_by_id.keys())
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let expected_staged_owner_count = usize::from(claim_is_registered);
    let mut distinct_resource_ids = HashSet::new();
    for resource in resources {
        let resource_id = resource.resource.id.as_str();
        if !distinct_resource_ids.insert(resource_id) {
            return Err(Status::invalid_argument(format!(
                "Duplicate task resource id: {}.",
                resource.resource.id
            )));
        }
        let staged_owner_count = inner
            .staged_resource_owner_counts
            .get(resource_id)
            .copied()
            .unwrap_or_default();
        if staged_owner_count < expected_staged_owner_count {
            return Err(Status::failed_precondition(
                "Task output resource staging ownership was lost.",
            ));
        }
        let current_task_resource = current_task_resources_by_id.get(resource_id);
        if current_task_resource.is_some_and(|existing| *existing != resource) {
            return Err(Status::invalid_argument(format!(
                "Task resource id cannot be reused for a different representation: {}.",
                resource.resource.id
            )));
        }
        let current_task_owns_id = current_task_resource.is_some();
        if staged_owner_count > expected_staged_owner_count
            || other_task_resource_ids.contains(resource_id)
            || (!current_task_owns_id
                && (visible_resource_ids.contains(resource_id)
                    || retained_resource_ids.contains(resource_id)
                    || inner.pending_resource_cleanup_ids.contains(resource_id)
                    || inner.durable_resource_cleanup_ids.contains(resource_id)))
        {
            return Err(Status::already_exists(format!(
                "Task resource id is already registered: {}.",
                resource.resource.id
            )));
        }
    }

    let mut projected_resource_ids = other_task_resource_ids
        .iter()
        .map(|resource_id| (*resource_id).to_owned())
        .collect::<HashSet<_>>();
    projected_resource_ids.extend(
        inner
            .visible_outputs_by_task_id
            .iter()
            .filter(|(candidate_task_id, _)| candidate_task_id.as_str() != task_id)
            .flat_map(|(_, output)| output.available_resources_by_id.keys().cloned()),
    );
    projected_resource_ids.extend(
        inner
            .retained_resource_snapshots
            .values()
            .flat_map(|snapshot| &snapshot.output.record.resources)
            .map(|resource| resource.resource.id.clone()),
    );
    projected_resource_ids.extend(inner.pending_resource_cleanup_ids.iter().cloned());
    projected_resource_ids.extend(inner.durable_resource_cleanup_ids.iter().cloned());
    projected_resource_ids.extend(inner.staged_resource_owner_counts.keys().cloned());
    projected_resource_ids.extend(candidate_resource_ids.iter().cloned());
    if projected_resource_ids.len() > MAX_REGISTERED_TASK_RESOURCES {
        return Err(Status::resource_exhausted(format!(
            "Task resource storage cannot register more than {MAX_REGISTERED_TASK_RESOURCES} resource ids."
        )));
    }
    Ok(())
}

fn release_staged_resource_owners_locked(
    inner: &mut RegistryInner,
    resource_ids: &HashSet<String>,
) {
    for resource_id in resource_ids {
        let remove_owner = {
            let count = inner
                .staged_resource_owner_counts
                .get_mut(resource_id)
                .expect("staged task resource owner must be registered");
            *count = count
                .checked_sub(1)
                .expect("staged task resource owner count must remain positive");
            *count == 0
        };
        if remove_owner {
            inner.staged_resource_owner_counts.remove(resource_id);
        }
    }
}

fn reserve_unowned_resource_cleanup_locked(
    inner: &mut RegistryInner,
    candidate_resource_ids: impl IntoIterator<Item = String>,
) -> bool {
    let reserved_resource_ids = reserved_resource_ids_locked(inner);
    let orphaned_resource_ids = candidate_resource_ids
        .into_iter()
        .filter(|resource_id| !reserved_resource_ids.contains(resource_id))
        .collect::<Vec<_>>();
    let cleanup_needed = !orphaned_resource_ids.is_empty();
    inner
        .durable_resource_cleanup_ids
        .extend(orphaned_resource_ids);
    cleanup_needed
}

fn shared_visible_task_output(
    previous: Option<&Arc<VisibleTaskOutput>>,
    record: &TaskOutputRecord,
) -> Arc<VisibleTaskOutput> {
    previous
        .filter(|output| {
            output.record.revision == record.revision
                && output.record.snapshot_id == record.snapshot_id
        })
        .cloned()
        .unwrap_or_else(|| Arc::new(VisibleTaskOutput::new(record.clone())))
}

fn reconcile_task_output_locked(
    inner: &mut RegistryInner,
    task_id: &str,
) -> Result<(), crate::task_output::TaskOutputValidationError> {
    let Some(task) = inner.tasks_by_id.get(task_id).cloned() else {
        inner.outputs_by_task_id.remove(task_id);
        return Ok(());
    };
    let summary = if let Some(output) = inner.outputs_by_task_id.get_mut(task_id) {
        output.reconcile_legacy_task(&task)?;
        output.summary()
    } else {
        let mut output = TaskOutputRecord::from_legacy_task(&task);
        output.reconcile_legacy_task(&task)?;
        let summary = output.summary();
        inner.outputs_by_task_id.insert(task_id.to_owned(), output);
        summary
    };
    let task = inner
        .tasks_by_id
        .get_mut(task_id)
        .expect("reconciled task must exist");
    let summary_changed = task.output_summary.as_ref() != Some(&summary);
    task.output_summary = Some(summary);
    if summary_changed {
        inner
            .pending_publications_by_id
            .insert(task_id.to_owned(), task.clone());
    }
    Ok(())
}

fn mark_output_playback_cache_deleted_locked(
    inner: &mut RegistryInner,
    task_id: &str,
    session_id: &str,
    library_item_id: &str,
    message: &str,
) -> Result<(), crate::task_output::TaskOutputValidationError> {
    let Some(output) = inner.outputs_by_task_id.get_mut(task_id) else {
        return Ok(());
    };
    let retired_ids = output.mark_playback_cache_deleted(session_id, library_item_id, message)?;
    let summary = output.summary();
    inner.pending_resource_cleanup_ids.extend(retired_ids);
    if let Some(task) = inner.tasks_by_id.get_mut(task_id) {
        task.output_summary = Some(summary);
    }
    Ok(())
}

fn persisted_records_locked(inner: &RegistryInner) -> Vec<PersistedTaskRecord> {
    let mut tasks = inner.tasks_by_id.values().cloned().collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        timestamp_sort_key(left.created_at.as_ref())
            .cmp(&timestamp_sort_key(right.created_at.as_ref()))
            .then_with(|| left.id.cmp(&right.id))
    });

    tasks
        .into_iter()
        .map(|task| PersistedTaskRecord {
            output: inner
                .outputs_by_task_id
                .get(&task.id)
                .cloned()
                .unwrap_or_else(|| TaskOutputRecord::from_legacy_task(&task)),
            options: inner
                .download_options_by_id
                .get(&task.id)
                .cloned()
                .flatten(),
            playback_options: inner
                .playback_options_by_id
                .get(&task.id)
                .cloned()
                .flatten(),
            task,
        })
        .collect()
}

fn terminal_task_ids_to_prune_locked(
    inner: &RegistryInner,
    policy: &TaskRetentionPolicy,
    now: &Timestamp,
) -> Vec<String> {
    let mut prunable_tasks = inner
        .tasks_by_id
        .values()
        .filter(|task| is_retention_prunable_terminal_task(task))
        .map(|task| {
            (
                task.id.clone(),
                timestamp_sort_key(terminal_task_retention_timestamp(task)),
            )
        })
        .collect::<Vec<_>>();

    let mut prune_ids = HashSet::new();
    if let Some(max_age) = policy.max_terminal_task_age {
        prune_ids.extend(
            inner
                .tasks_by_id
                .values()
                .filter(|task| {
                    is_retention_prunable_terminal_task(task)
                        && terminal_task_age_exceeds(task, now, max_age)
                })
                .map(|task| task.id.clone()),
        );
    }

    if let Some(max_terminal_tasks) = policy.max_terminal_tasks {
        prunable_tasks.retain(|(task_id, _)| !prune_ids.contains(task_id));
        if prunable_tasks.len() > max_terminal_tasks {
            prunable_tasks
                .sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
            let remove_count = prunable_tasks.len() - max_terminal_tasks;
            prune_ids.extend(
                prunable_tasks
                    .into_iter()
                    .take(remove_count)
                    .map(|(task_id, _)| task_id),
            );
        }
    }

    prune_ids.into_iter().collect()
}

fn apply_pruned_tasks_locked(inner: &mut RegistryInner, task_ids: &[String]) {
    for task_id in task_ids {
        inner.tasks_by_id.remove(task_id);
        inner.outputs_by_task_id.remove(task_id);
        inner.download_options_by_id.remove(task_id);
        inner.playback_options_by_id.remove(task_id);
        inner.running_cancellations_by_id.remove(task_id);
        inner.planning_cancellations_by_id.remove(task_id);
    }
}

fn is_retention_prunable_terminal_task(task: &Task) -> bool {
    is_terminal(task.state())
        && !(task.kind() == TaskKind::BilibiliProgressivePlayback
            && task.state() == TaskState::Completed)
}

fn terminal_task_retention_timestamp(task: &Task) -> Option<&Timestamp> {
    task.finished_at
        .as_ref()
        .or(task.updated_at.as_ref())
        .or(task.created_at.as_ref())
}

fn terminal_task_age_exceeds(task: &Task, now: &Timestamp, max_age: Duration) -> bool {
    let Some(reference) = terminal_task_retention_timestamp(task) else {
        return false;
    };
    timestamp_age_nanos(now, reference).is_some_and(|age| age >= duration_nanos(max_age))
}

fn timestamp_age_nanos(now: &Timestamp, reference: &Timestamp) -> Option<i128> {
    timestamp_nanos(now).checked_sub(timestamp_nanos(reference))
}

fn timestamp_nanos(timestamp: &Timestamp) -> i128 {
    i128::from(timestamp.seconds) * 1_000_000_000 + i128::from(timestamp.nanos)
}

fn duration_nanos(duration: Duration) -> i128 {
    i128::from(duration.as_secs()) * 1_000_000_000 + i128::from(duration.subsec_nanos())
}

fn timestamp_sort_key(timestamp: Option<&Timestamp>) -> (i64, i32) {
    timestamp
        .map(|timestamp| (timestamp.seconds, timestamp.nanos))
        .unwrap_or_default()
}

pub fn current_timestamp() -> Timestamp {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("current time must be after unix epoch");
    Timestamp {
        seconds: now.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: now.subsec_nanos().try_into().unwrap_or(i32::MAX),
    }
}

fn copy_timestamp(timestamp: &Timestamp) -> Timestamp {
    Timestamp {
        seconds: timestamp.seconds,
        nanos: timestamp.nanos,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_checkpoint_restores_pending_resource_cleanup_reservations() {
        let mut inner = RegistryInner::default();
        inner
            .pending_resource_cleanup_ids
            .insert("existing-resource".to_owned());
        let checkpoint = RegistryMutationCheckpoint::capture(&inner);

        inner
            .pending_resource_cleanup_ids
            .insert("rolled-back-resource".to_owned());
        checkpoint.restore(&mut inner);

        assert_eq!(
            HashSet::from(["existing-resource".to_owned()]),
            inner.pending_resource_cleanup_ids
        );
    }

    #[test]
    fn dedupes_active_tasks_by_source_and_options() {
        let registry = BilibiliTaskRegistry::default();
        let first = registry
            .create_bilibili_task("  BV1xx  ", Some(download_options("720P", false)))
            .expect("task should be created");
        let duplicate = registry
            .create_bilibili_task("BV1xx", Some(download_options("720p", false)))
            .expect("duplicate task should be returned");
        let different_quality = registry
            .create_bilibili_task("BV1xx", Some(download_options("1080p", false)))
            .expect("task with different options should be created");
        let different_subtitles = registry
            .create_bilibili_task("BV1xx", Some(download_options("720p", true)))
            .expect("task with different subtitle options should be created");

        assert_eq!(first.id, duplicate.id);
        assert_ne!(first.id, different_quality.id);
        assert_ne!(first.id, different_subtitles.id);
    }

    #[test]
    fn creates_request_scoped_progressive_playback_tasks_without_colliding_with_downloads() {
        let registry = BilibiliTaskRegistry::default();
        let download = registry
            .create_bilibili_task("BV1play", None)
            .expect("download task should be created");
        let playback = registry
            .create_bilibili_playback_task("BV1play", Some(playback_options("720P")), None)
            .expect("playback task should be created");
        let repeated_playback = registry
            .create_bilibili_playback_task("  BV1play  ", Some(playback_options("720p")), None)
            .expect("repeated playback task should be created");
        let different_playback = registry
            .create_bilibili_playback_task("BV1play", Some(playback_options("1080p")), None)
            .expect("different playback task should be created");

        assert_ne!(download.id, playback.task.id);
        assert!(playback.created);
        assert!(repeated_playback.created);
        assert_ne!(playback.task.id, repeated_playback.task.id);
        assert_ne!(playback.task.id, different_playback.task.id);
    }

    #[test]
    fn preparing_playback_cancel_sets_token_and_late_plan_stays_cancelled() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1cancel-planning", None, None)
            .expect("playback task should be created");
        let cancellation = created
            .cancellation
            .clone()
            .expect("new playback task should expose cancellation");

        let cancel_requested = registry
            .cancel_task(&created.task.id)
            .expect("planning task should be cancellable");

        assert_eq!(TaskState::CancelRequested, cancel_requested.state());
        assert!(cancellation.is_cancel_requested());
        assert!(registry.is_cancel_requested(&created.task.id));

        let final_task = registry
            .complete_playback_playable(
                &created.task.id,
                "Late playback plan".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("late planning completion should be reconciled");

        assert_eq!(TaskState::Cancelled, final_task.state());
        assert_eq!(CANCELLED_BY_REQUEST_MESSAGE, final_task.message);
        assert!(final_task.playback_source.is_none());
        assert!(final_task.playback_session.is_none());
        assert!(!registry.is_cancel_requested(&created.task.id));

        let recreated = registry
            .create_bilibili_playback_task("BV1cancel-planning", None, None)
            .expect("cancelled source should be requeueable");
        assert!(recreated.created);
        assert_ne!(created.task.id, recreated.task.id);
    }

    #[test]
    fn cancel_is_idempotent_and_allows_requeue() {
        let registry = BilibiliTaskRegistry::default();
        let first = registry
            .create_bilibili_task("BV1yy", None)
            .expect("task should be created");

        let cancelled = registry.cancel_task(&first.id).expect("cancel should work");
        assert_eq!(TaskState::Cancelled, cancelled.state());
        let cancelled_again = registry
            .cancel_task(&first.id)
            .expect("cancel is idempotent");
        assert_eq!(cancelled.id, cancelled_again.id);

        let requeued = registry
            .create_bilibili_task("BV1yy", None)
            .expect("source can be queued after cancellation");
        assert_ne!(first.id, requeued.id);
    }

    #[tokio::test]
    async fn unrelated_updates_do_not_lag_filtered_subscriptions() {
        let registry = BilibiliTaskRegistry::default();
        let watched = registry
            .create_bilibili_task("BV1watched", None)
            .expect("watched task should be created");
        let mut subscription = registry
            .subscribe(std::slice::from_ref(&watched.id))
            .expect("subscription should be created");

        for index in 0..=WATCHER_EVENT_BUFFER_CAPACITY {
            registry
                .create_bilibili_task(&format!("BV1unrelated-{index}"), None)
                .expect("unrelated task should be created");
        }

        let cancelled = registry
            .cancel_task(&watched.id)
            .expect("watched task should be cancelled");
        let event = subscription
            .recv()
            .await
            .expect("filtered subscription should not lag on unrelated updates");

        assert_eq!(cancelled.id, event.id);
        assert_eq!(TaskState::Cancelled, event.state());
    }

    #[tokio::test]
    async fn claim_moves_queued_task_to_running_and_reports_progress() {
        let registry = BilibiliTaskRegistry::default();
        let task = registry
            .create_bilibili_task("BV1worker", None)
            .expect("task should be created");

        let work_item = registry.claim_next_bilibili_task().await;

        assert_eq!(task.id, work_item.task_id);
        assert_eq!("BV1worker", work_item.source);
        let running = registry.get_task(&task.id).expect("task should exist");
        assert_eq!(TaskState::Running, running.state());

        assert!(registry.update_task_progress(
            &task.id,
            BilibiliTaskProgress {
                progress: Some(0.25),
                downloaded_bytes: Some(1024),
                total_bytes: Some(4096),
                message: Some("Downloading video stream.".to_owned()),
            },
        ));

        let updated = registry.get_task(&task.id).expect("task should exist");
        assert_eq!(TaskState::Running, updated.state());
        assert_eq!(0.25, updated.progress);
        assert_eq!(1024, updated.downloaded_bytes);
        assert_eq!(4096, updated.total_bytes);
        assert_eq!("Downloading video stream.", updated.message);
    }

    #[tokio::test]
    async fn running_cancel_sets_cancel_requested_and_token() {
        let registry = BilibiliTaskRegistry::default();
        let task = registry
            .create_bilibili_task("BV1cancel-running", None)
            .expect("task should be created");
        let work_item = registry.claim_next_bilibili_task().await;

        let cancelled = registry.cancel_task(&task.id).expect("cancel should work");

        assert_eq!(TaskState::CancelRequested, cancelled.state());
        assert!(work_item.cancellation.is_cancel_requested());
        assert!(registry.is_cancel_requested(&task.id));

        let final_task = registry
            .complete_task_cancelled(&task.id, "Cancelled by adapter.".to_owned())
            .expect("completion should work");
        assert_eq!(TaskState::Cancelled, final_task.state());
        assert!(!registry.is_cancel_requested(&task.id));
    }

    #[tokio::test]
    async fn cancel_requested_wins_over_late_success_completion() {
        let registry = BilibiliTaskRegistry::default();
        let task = registry
            .create_bilibili_task("BV1cancel-race", None)
            .expect("task should be created");
        let _ = registry.claim_next_bilibili_task().await;
        let cancel_requested = registry.cancel_task(&task.id).expect("cancel should work");
        assert_eq!(TaskState::CancelRequested, cancel_requested.state());

        let final_task = registry
            .complete_task_succeeded(
                &task.id,
                "library-item-after-cancel".to_owned(),
                "Completed after cancellation.".to_owned(),
            )
            .expect("late completion should be reconciled");

        assert_eq!(TaskState::Cancelled, final_task.state());
        assert_eq!(CANCELLED_BY_REQUEST_MESSAGE, final_task.message);
        assert!(final_task.library_item_id.is_empty());
        assert!(!registry.is_cancel_requested(&task.id));
    }

    #[tokio::test]
    async fn restores_queued_task_with_options_from_disk() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let options = BilibiliDownloadOptions {
            quality_preference: "1080p".to_owned(),
            encoding_preference: "hevc".to_owned(),
            prefer_tv_api: true,
            download_subtitles: true,
            download_danmaku: false,
            audio_language: "ja-jp".to_owned(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::PreferNonAi.into(),
            download_cover: true,
            danmaku_formats: vec![BilibiliDanmakuFormat::Xml.into()],
        };
        let task = BilibiliTaskRegistry::with_persistence_path(&path)
            .create_bilibili_task("BV1persist", Some(options.clone()))
            .expect("task should be created");

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored.get_task(&task.id).expect("task should restore");
        let work_item = restored
            .try_claim_next_bilibili_task()
            .expect("restored queued task should be claimable");

        assert_eq!(TaskState::Queued, restored_task.state());
        assert_eq!(task.id, work_item.task_id);
        assert_eq!(Some(options), work_item.options);
    }

    #[test]
    fn restores_planned_progressive_playback_task_without_deduping_new_requests() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let options = playback_options("1080p");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1planned", Some(options.clone()), None)
            .expect("playback task should be created");
        let planned = registry
            .complete_playback_planned(
                &created.task.id,
                "Planned playback".to_owned(),
                playback_session(&created.task.id),
            )
            .expect("planning should complete");

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored.get_task(&planned.id).expect("task should restore");
        let duplicate = restored
            .create_bilibili_playback_task("BV1planned", Some(options), None)
            .expect("new request-scoped playback task should be created");

        assert_eq!(TaskState::Planned, restored_task.state());
        assert!(restored_task.playback_source.is_none());
        assert_eq!("cid-1", restored_task.playback_session.unwrap().content_id);
        assert!(!registry.is_cancel_requested(&planned.id));
        assert_ne!(planned.id, duplicate.task.id);
        assert!(duplicate.created);
    }

    #[test]
    fn restores_playable_progressive_playback_task_after_restart() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1playable", Some(playback_options("1080p")), None)
            .expect("playback task should be created");
        let playable = registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored
            .get_task(&playable.id)
            .expect("task should restore");
        let requeued = restored
            .create_bilibili_playback_task("BV1playable", Some(playback_options("1080p")), None)
            .expect("playable source should not dedupe future requests");

        assert_eq!(TaskState::Playable, restored_task.state());
        assert!(restored_task.playback_source.is_some());
        assert_ne!(playable.id, requeued.task.id);
    }

    #[test]
    fn completed_progressive_playback_cache_rewrites_runtime_source_to_library_item() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1completed", Some(playback_options("1080p")), None)
            .expect("playback task should be created");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");

        let completed = registry
            .complete_playback_cached(&created.task.id, "bilibili.hls.completed".to_owned())
            .expect("playback cache should complete");

        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!("bilibili.hls.completed", completed.library_item_id);
        let completed_source = completed
            .playback_source
            .as_ref()
            .expect("completed playback should keep a source");
        assert_eq!("bilibili.hls.completed", completed_source.item_id);

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored
            .get_task(&completed.id)
            .expect("completed playback task should restore");

        assert_eq!(TaskState::Completed, restored_task.state());
        assert_eq!("bilibili.hls.completed", restored_task.library_item_id);
        let restored_source = restored_task
            .playback_source
            .as_ref()
            .expect("restored completed playback should keep a source");
        assert_eq!("bilibili.hls.completed", restored_source.item_id);
    }

    #[test]
    fn remove_completed_playback_task_deletes_matching_completed_record() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1delete", Some(playback_options("1080p")), None)
            .expect("playback task should be created");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");
        let library_item_id = format!("bilibili.hls.{}", created.task.id);
        registry
            .complete_playback_cached(&created.task.id, library_item_id.clone())
            .expect("playback cache should complete");
        let durable_task = registry.get_task(&created.task.id).unwrap();
        let durable_output = registry.task_output_snapshot(&created.task.id).unwrap();
        let durable_state = std::fs::read(&path).expect("durable state should be readable");

        std::fs::remove_file(&path).expect("state file should be removable");
        std::fs::create_dir(&path).expect("directory should block snapshot replacement");
        let error = registry
            .remove_completed_playback_task(&created.task.id, &library_item_id)
            .expect_err("whole-task deletion must fail when durability fails");
        assert_eq!(tonic::Code::Unavailable, error.code());
        assert_eq!(durable_task, registry.get_task(&created.task.id).unwrap());
        let visible_output = registry.task_output_snapshot(&created.task.id).unwrap();
        assert_eq!(durable_output.revision, visible_output.revision);
        assert_eq!(
            durable_output.output.record.results,
            visible_output.output.record.results
        );

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        std::fs::write(&path, durable_state).expect("durable state should be restored");

        assert!(
            registry
                .remove_completed_playback_task(&created.task.id, &library_item_id)
                .expect("completed playback task should be removable")
        );
        assert!(registry.get_task(&created.task.id).is_err());

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        assert!(restored.get_task(&created.task.id).is_err());
    }

    #[tokio::test]
    async fn remove_completed_playback_task_notifies_watchers_before_record_removal() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1delete", Some(playback_options("1080p")), None)
            .expect("playback task should be created");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");
        let library_item_id = format!("bilibili.hls.{}", created.task.id);
        registry
            .complete_playback_cached(&created.task.id, library_item_id.clone())
            .expect("playback cache should complete");
        let mut subscription = registry
            .subscribe(std::slice::from_ref(&created.task.id))
            .expect("subscription should be created");
        let previous_revision = subscription.snapshots()[0]
            .output_summary
            .as_ref()
            .expect("completed task should have an output summary")
            .revision;

        assert_eq!(1, subscription.snapshots().len());
        assert!(
            registry
                .remove_completed_playback_task(&created.task.id, &library_item_id)
                .expect("completed playback task should be removable")
        );

        let event = subscription
            .recv()
            .await
            .expect("watcher should receive task removal tombstone");

        assert_eq!(created.task.id, event.id);
        assert_eq!(TaskState::Failed, event.state());
        assert_eq!(PLAYBACK_CACHE_DELETED_MESSAGE, event.message);
        assert!(event.library_item_id.is_empty());
        assert!(event.playback_source.is_none());
        assert!(event.playback_session.is_none());
        let summary = event
            .output_summary
            .expect("removal tombstone should carry a failed output summary");
        assert!(summary.revision > previous_revision);
        assert_eq!(1, summary.failed_result_count);
        assert_eq!(0, summary.successful_result_count);
        assert_eq!(0, summary.available_artifact_count);
        assert!(registry.get_task(&created.task.id).is_err());
    }

    #[test]
    fn remove_completed_playback_task_rejects_mismatched_or_active_records() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1active", Some(playback_options("1080p")), None)
            .expect("playback task should be created");

        let active_error = registry
            .remove_completed_playback_task(&created.task.id, "bilibili.hls.active")
            .expect_err("active task should not be removable");
        assert_eq!(tonic::Code::FailedPrecondition, active_error.code());

        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");
        let library_item_id = format!("bilibili.hls.{}", created.task.id);
        registry
            .complete_playback_cached(&created.task.id, library_item_id)
            .expect("playback cache should complete");

        let mismatch_error = registry
            .remove_completed_playback_task(&created.task.id, "bilibili.hls.other")
            .expect_err("mismatched item should not be removable");
        assert_eq!(tonic::Code::FailedPrecondition, mismatch_error.code());
        assert!(registry.get_task(&created.task.id).is_ok());
    }

    #[test]
    fn refresh_playback_source_updates_restored_progressive_task_uri() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1refresh", Some(playback_options("1080p")), None)
            .expect("playback task should be created");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");
        let mut refreshed_source = playback_source(&created.task.id);
        refreshed_source.uri = format!(
            "http://restored.example.test:9090/hls/{}/master.m3u8",
            created.task.id
        );

        let refreshed = registry
            .refresh_playback_source(&created.task.id, refreshed_source.clone())
            .expect("playback source should refresh");

        assert_eq!(TaskState::Playable, refreshed.state());
        assert_eq!(Some(refreshed_source), refreshed.playback_source);
    }

    #[test]
    fn refresh_hls_playback_source_updates_secondary_result_uri() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1refresh-result",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let child_source = playback_source(&child_session_id);
        let child_session = playback_session(&child_session_id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Failed.into(),
                message: "page 1 planning failed".to_owned(),
                library_item_id: String::new(),
                playback_source: None,
                playback_session: None,
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(child_source.clone()),
                playback_session: Some(child_session.clone()),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Partially playable".to_owned(),
                "1/2 Bilibili playback result(s) are playable.".to_owned(),
                child_source,
                child_session,
                result_items,
            )
            .expect("playback results should become playable");
        let mut refreshed_source = playback_source(&child_session_id);
        let expected_uri =
            format!("http://restored.example.test:9090/hls/{child_session_id}/master.m3u8");
        refreshed_source.uri = expected_uri.clone();

        let refreshed = registry
            .refresh_hls_playback_source(&child_session_id, refreshed_source.clone())
            .expect("secondary playback source should refresh");

        let mut expected_primary_source = refreshed_source.clone();
        expected_primary_source.item_id = created.task.id.clone();
        assert_eq!(TaskState::Playable, refreshed.state());
        assert_eq!(Some(expected_primary_source), refreshed.playback_source);
        assert_eq!(2, refreshed.result_items.len());
        assert!(refreshed.result_items[0].playback_source.is_none());
        assert_eq!(
            Some(refreshed_source),
            refreshed.result_items[1].playback_source
        );
        assert_eq!(
            Some(expected_uri),
            registry.hls_playback_source_uri(&child_session_id)
        );
    }

    #[test]
    fn fails_playable_task_when_primary_hls_session_is_unrestorable() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1missing-primary",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                result_items,
            )
            .expect("playback results should become playable");

        let failed_ids = registry
            .fail_unrestorable_playback_tasks(&HashSet::from([child_session_id]), &HashSet::new());
        let failed = registry
            .get_task(&created.task.id)
            .expect("failed task should remain readable");

        assert_eq!(vec![created.task.id], failed_ids);
        assert_eq!(TaskState::Failed, failed.state());
        assert!(failed.playback_source.is_none());
        assert!(failed.playback_session.is_none());
        assert!(
            failed
                .result_items
                .iter()
                .all(|item| { item.playback_source.is_none() && item.playback_session.is_none() })
        );
    }

    #[test]
    fn clears_unrestorable_secondary_result_but_keeps_primary_playable() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1missing-secondary",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                result_items,
            )
            .expect("playback results should become playable");

        let changed_ids = registry.fail_unrestorable_playback_tasks(
            &HashSet::from([created.task.id.clone()]),
            &HashSet::new(),
        );
        let playable = registry
            .get_task(&created.task.id)
            .expect("playable task should remain readable");

        assert_eq!(vec![created.task.id], changed_ids);
        assert_eq!(TaskState::Playable, playable.state());
        assert!(playable.playback_source.is_some());
        assert!(playable.playback_session.is_some());
        assert_eq!(
            i32::from(TaskState::Playable),
            playable.result_items[0].state
        );
        assert!(playable.result_items[0].playback_source.is_some());
        assert!(playable.result_items[0].playback_session.is_some());
        assert_eq!(i32::from(TaskState::Failed), playable.result_items[1].state);
        assert!(playable.result_items[1].playback_source.is_none());
        assert!(playable.result_items[1].playback_session.is_none());
    }

    #[test]
    fn clears_unrestorable_secondary_result_but_keeps_primary_completed() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1completed-missing-secondary",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&child_session_id),
                playback_session(&child_session_id),
                result_items,
            )
            .expect("playback results should become playable");
        let library_item_id = format!("bilibili.hls.{child_session_id}");
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &child_session_id,
                library_item_id.clone(),
            )
            .expect("primary child session should become completed");

        let changed_ids = registry.fail_unrestorable_playback_tasks(
            &HashSet::new(),
            &HashSet::from([child_session_id.clone()]),
        );
        let completed = registry
            .get_task(&created.task.id)
            .expect("completed task should remain readable");

        assert_eq!(vec![created.task.id], changed_ids);
        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(library_item_id, completed.library_item_id);
        assert_eq!(2, completed.result_items.len());
        assert_eq!(
            i32::from(TaskState::Failed),
            completed.result_items[0].state
        );
        assert!(completed.result_items[0].playback_source.is_none());
        assert!(completed.result_items[0].playback_session.is_none());
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[1].state
        );
        assert_eq!(library_item_id, completed.result_items[1].library_item_id);
        assert!(completed.result_items[1].playback_source.is_some());
        assert!(completed.result_items[1].playback_session.is_some());
    }

    #[test]
    fn cache_restore_missing_primary_result_session_fails_playable_task() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1missing-primary-result",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let child_source = playback_source(&child_session_id);
        let child_session = playback_session(&child_session_id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Failed.into(),
                message: "page 1 planning failed".to_owned(),
                library_item_id: String::new(),
                playback_source: None,
                playback_session: None,
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(child_source.clone()),
                playback_session: Some(child_session.clone()),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Partially playable".to_owned(),
                "1/2 Bilibili playback result(s) are playable.".to_owned(),
                child_source,
                child_session,
                result_items,
            )
            .expect("playback results should become playable");

        let failed = registry
            .fail_unrestorable_playback_session_after_cache_restore(
                &child_session_id,
                "missing".to_owned(),
            )
            .expect("missing primary should be handled")
            .expect("missing primary should update task");

        assert_eq!(TaskState::Failed, failed.state());
        assert_eq!("missing", failed.message);
        assert!(failed.playback_source.is_none());
        assert!(failed.playback_session.is_none());
        assert!(
            failed
                .result_items
                .iter()
                .all(|item| item.playback_source.is_none() && item.playback_session.is_none())
        );
    }

    #[test]
    fn cache_restore_missing_secondary_result_session_keeps_primary_playable() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1missing-secondary-lazy",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                result_items,
            )
            .expect("playback results should become playable");

        let playable = registry
            .fail_unrestorable_playback_session_after_cache_restore(
                &child_session_id,
                "missing".to_owned(),
            )
            .expect("missing secondary should be handled")
            .expect("missing secondary should update task");

        assert_eq!(TaskState::Playable, playable.state());
        assert!(playable.playback_source.is_some());
        assert!(playable.playback_session.is_some());
        assert_eq!(
            i32::from(TaskState::Playable),
            playable.result_items[0].state
        );
        assert!(playable.result_items[0].playback_source.is_some());
        assert!(playable.result_items[0].playback_session.is_some());
        assert_eq!(i32::from(TaskState::Failed), playable.result_items[1].state);
        assert_eq!("missing", playable.result_items[1].message);
        assert!(playable.result_items[1].playback_source.is_none());
        assert!(playable.result_items[1].playback_session.is_none());
    }

    #[test]
    fn cache_fill_failure_keeps_primary_playback_task_playable() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1cache-fill-primary-failed", None, None)
            .expect("playback task should be created");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");

        let playable = registry
            .fail_hls_cache_fill_for_playback_session(
                &created.task.id,
                &created.task.id,
                "Playable online; offline cache fill failed: upstream failed".to_owned(),
            )
            .expect("cache fill failure should be handled")
            .expect("cache fill failure should update task");

        assert_eq!(TaskState::Playable, playable.state());
        assert_eq!(
            "Playable online; offline cache fill failed: upstream failed",
            playable.message
        );
        assert!(playable.playback_source.is_some());
        assert!(playable.playback_session.is_some());
        assert!(
            registry.hls_session_has_online_playback_after_cache_fill_failure(
                &created.task.id,
                &created.task.id,
            )
        );
    }

    #[test]
    fn cache_fill_failure_keeps_primary_result_degraded_after_secondary_updates() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1primary-degraded-secondary-updates", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                vec![
                    BilibiliTaskResultItem {
                        id: created.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&created.task.id)),
                        playback_session: Some(playback_session(&created.task.id)),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&child_session_id)),
                        playback_session: Some(playback_session(&child_session_id)),
                    },
                ],
            )
            .expect("playback results should become playable");

        registry
            .fail_hls_cache_fill_for_playback_session(
                &created.task.id,
                &created.task.id,
                "Playable online; offline cache fill failed: upstream failed".to_owned(),
            )
            .expect("primary cache fill failure should be handled")
            .expect("primary cache fill failure should update task");
        let updated = registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &child_session_id,
                format!("bilibili.hls.{child_session_id}"),
            )
            .expect("secondary session should become completed");

        assert_eq!(TaskState::Playable, updated.state());
        assert_eq!(
            "Playable online; selected Bilibili playback results are cached offline.",
            updated.message
        );
        assert_eq!(i32::from(TaskState::Failed), updated.result_items[0].state);
        assert_eq!(
            "Playable online; offline cache fill failed: upstream failed",
            updated.result_items[0].message
        );
        assert!(updated.result_items[0].playback_source.is_some());
        assert!(updated.result_items[0].playback_session.is_some());
        assert_eq!(
            i32::from(TaskState::Completed),
            updated.result_items[1].state
        );
        assert!(
            registry.hls_session_has_online_playback_after_cache_fill_failure(
                &created.task.id,
                &created.task.id,
            ),
            "primary degraded state must survive task-level message updates"
        );
    }

    #[test]
    fn cache_fill_failure_marks_completed_secondary_result_failed_but_keeps_playback() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1cache-fill-secondary-failed", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                vec![
                    BilibiliTaskResultItem {
                        id: created.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&created.task.id)),
                        playback_session: Some(playback_session(&created.task.id)),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&child_session_id)),
                        playback_session: Some(playback_session(&child_session_id)),
                    },
                ],
            )
            .expect("playback results should become playable");
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &created.task.id,
                format!("bilibili.hls.{}", created.task.id),
            )
            .expect("primary session should become completed");

        let updated = registry
            .fail_hls_cache_fill_for_playback_session(
                &created.task.id,
                &child_session_id,
                "Playable online; offline cache fill failed: upstream failed".to_owned(),
            )
            .expect("secondary cache fill failure should be handled")
            .expect("secondary cache fill failure should update task");

        assert_eq!(TaskState::Completed, updated.state());
        assert_eq!(
            "Completed offline; some Bilibili playback results failed to cache offline.",
            updated.message
        );
        assert_eq!(
            i32::from(TaskState::Completed),
            updated.result_items[0].state
        );
        assert_eq!(i32::from(TaskState::Failed), updated.result_items[1].state);
        assert_eq!(
            "Playable online; offline cache fill failed: upstream failed",
            updated.result_items[1].message
        );
        assert!(updated.result_items[1].library_item_id.is_empty());
        assert!(updated.result_items[1].playback_source.is_some());
        assert!(updated.result_items[1].playback_session.is_some());
        assert_eq!(
            Some(created.task.id.clone()),
            registry.playable_task_id_for_hls_session(&child_session_id)
        );
        assert!(registry.is_playback_result_session_playable(&child_session_id, true));
        assert!(
            registry.hls_session_has_online_playback_after_cache_fill_failure(
                &created.task.id,
                &child_session_id,
            )
        );

        let changed_ids = registry.fail_unrestorable_playback_tasks(
            &HashSet::from([child_session_id.clone()]),
            &HashSet::from([created.task.id.clone()]),
        );
        let restored = registry
            .get_task(&created.task.id)
            .expect("completed parent task should remain readable");

        assert!(changed_ids.is_empty());
        assert_eq!(TaskState::Completed, restored.state());
        assert_eq!(i32::from(TaskState::Failed), restored.result_items[1].state);
        assert_eq!(
            "Playable online; offline cache fill failed: upstream failed",
            restored.result_items[1].message
        );
        assert!(restored.result_items[1].playback_source.is_some());
        assert!(restored.result_items[1].playback_session.is_some());

        let expired = registry
            .fail_unrestorable_playback_session_after_cache_restore(
                &child_session_id,
                "Failed to restore offline HLS cache after restart: missing manifest".to_owned(),
            )
            .expect("single-session restore failure should be handled")
            .expect("failed-but-playable secondary result should be updated");
        assert_eq!(TaskState::Completed, expired.state());
        assert_eq!(i32::from(TaskState::Failed), expired.result_items[1].state);
        assert_eq!(
            "Failed to restore offline HLS cache after restart: missing manifest",
            expired.result_items[1].message
        );
        assert!(expired.result_items[1].library_item_id.is_empty());
        assert!(expired.result_items[1].playback_source.is_none());
        assert!(expired.result_items[1].playback_session.is_none());
        assert!(
            registry
                .playable_task_id_for_hls_session(&child_session_id)
                .is_none()
        );
        assert!(!registry.is_playback_result_session_playable(&child_session_id, true));
    }

    #[test]
    fn cache_restore_missing_completed_secondary_result_keeps_parent_completed() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1restore-secondary-failed", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                vec![
                    BilibiliTaskResultItem {
                        id: created.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&created.task.id)),
                        playback_session: Some(playback_session(&created.task.id)),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&child_session_id)),
                        playback_session: Some(playback_session(&child_session_id)),
                    },
                ],
            )
            .expect("playback results should become playable");
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &created.task.id,
                format!("bilibili.hls.{}", created.task.id),
            )
            .expect("primary session should become completed");

        let updated = registry
            .fail_unrestorable_playback_session_after_cache_restore(
                &child_session_id,
                "missing restored secondary".to_owned(),
            )
            .expect("missing secondary should be handled")
            .expect("missing secondary should update task");

        assert_eq!(TaskState::Completed, updated.state());
        assert_eq!(
            "Completed offline cache restored; some Bilibili playback results expired after cache restore.",
            updated.message
        );
        assert_eq!(
            i32::from(TaskState::Completed),
            updated.result_items[0].state
        );
        assert_eq!(i32::from(TaskState::Failed), updated.result_items[1].state);
        assert_eq!(
            "missing restored secondary",
            updated.result_items[1].message
        );
        assert!(updated.result_items[1].playback_source.is_none());
        assert!(updated.result_items[1].playback_session.is_none());
    }

    #[test]
    fn keeps_restorable_secondary_result_when_primary_completed() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1completed-restored-secondary",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                result_items,
            )
            .expect("playback results should become playable");
        let library_item_id = format!("bilibili.hls.{}", created.task.id);
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &created.task.id,
                library_item_id.clone(),
            )
            .expect("primary session should become completed");

        let changed_ids = registry.fail_unrestorable_playback_tasks(
            &HashSet::from([child_session_id.clone()]),
            &HashSet::from([created.task.id.clone()]),
        );
        let completed = registry
            .get_task(&created.task.id)
            .expect("completed task should remain readable");

        assert!(changed_ids.is_empty());
        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(library_item_id, completed.library_item_id);
        assert_eq!(2, completed.result_items.len());
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[0].state
        );
        assert_eq!(
            i32::from(TaskState::Playable),
            completed.result_items[1].state
        );
        assert!(completed.result_items[1].library_item_id.is_empty());
        assert!(completed.result_items[1].playback_source.is_some());
        assert!(completed.result_items[1].playback_session.is_some());
    }

    #[test]
    fn clears_stale_completed_secondary_result_when_completed_cache_item_is_missing() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1completed-stale-secondary-cache",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                result_items,
            )
            .expect("playback results should become playable");
        let primary_library_item_id = format!("bilibili.hls.{}", created.task.id);
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &created.task.id,
                primary_library_item_id.clone(),
            )
            .expect("primary session should become completed");
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &child_session_id,
                format!("bilibili.hls.{child_session_id}"),
            )
            .expect("secondary session should become completed");

        let changed_ids = registry.fail_unrestorable_playback_tasks(
            &HashSet::from([child_session_id.clone()]),
            &HashSet::from([created.task.id.clone()]),
        );
        let completed = registry
            .get_task(&created.task.id)
            .expect("completed task should remain readable");

        assert_eq!(vec![created.task.id], changed_ids);
        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(primary_library_item_id, completed.library_item_id);
        assert_eq!(
            "Completed offline cache restored; some Bilibili playback results expired after restart.",
            completed.message
        );
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[0].state
        );
        assert_eq!(
            i32::from(TaskState::Failed),
            completed.result_items[1].state
        );
        assert!(completed.result_items[1].library_item_id.is_empty());
        assert!(completed.result_items[1].playback_source.is_none());
        assert!(completed.result_items[1].playback_session.is_none());
    }

    #[test]
    fn hls_completion_caches_secondary_result_after_primary_completed() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1completed-secondary-cache", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                result_items,
            )
            .expect("multi-result playback task should become playable");
        let primary_library_item_id = format!("bilibili.hls.{}", created.task.id);
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &created.task.id,
                primary_library_item_id.clone(),
            )
            .expect("primary session should become completed");
        let secondary_library_item_id = format!("bilibili.hls.{child_session_id}");

        let completed = registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &child_session_id,
                secondary_library_item_id.clone(),
            )
            .expect("secondary session should become completed");

        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(primary_library_item_id, completed.library_item_id);
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[0].state
        );
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[1].state
        );
        assert_eq!(
            secondary_library_item_id,
            completed.result_items[1].library_item_id
        );
        assert!(registry.playback_task_has_completed_hls_cache_item(
            &completed,
            &child_session_id,
            &secondary_library_item_id
        ));
    }

    #[test]
    fn hls_completion_caches_secondary_result_without_completing_parent() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1playable-secondary-cache", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                result_items,
            )
            .expect("multi-result playback task should become playable");
        let secondary_library_item_id = format!("bilibili.hls.{child_session_id}");

        let playable = registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &child_session_id,
                secondary_library_item_id.clone(),
            )
            .expect("secondary session should become completed");

        assert_eq!(TaskState::Playable, playable.state());
        assert!(playable.library_item_id.is_empty());
        assert_eq!(
            i32::from(TaskState::Playable),
            playable.result_items[0].state
        );
        assert_eq!(
            i32::from(TaskState::Completed),
            playable.result_items[1].state
        );
        assert_eq!(
            secondary_library_item_id,
            playable.result_items[1].library_item_id
        );
        assert!(registry.playback_task_has_completed_hls_cache_item(
            &playable,
            &child_session_id,
            &secondary_library_item_id
        ));
    }

    #[test]
    fn hls_completion_updates_completed_playback_session_metadata() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1completed-generated-metadata", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let mut completed_session = playback_session(&child_session_id);
        completed_session.title = "Generated playback".to_owned();
        completed_session.selected_variant_id = "h264-generated".to_owned();
        if let Some(selected_variant) = completed_session.selected_variant.as_mut() {
            selected_variant.id = "h264-generated".to_owned();
            selected_variant.video_codec = "avc1.64002A".to_owned();
            selected_variant.audio_codec = "mp4a.40.2".to_owned();
            selected_variant.size_bytes = 42;
        }
        completed_session.variants = completed_session
            .selected_variant
            .clone()
            .into_iter()
            .collect();
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&child_session_id),
                playback_session(&child_session_id),
                vec![
                    BilibiliTaskResultItem {
                        id: created.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&created.task.id)),
                        playback_session: Some(playback_session(&created.task.id)),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&child_session_id)),
                        playback_session: Some(playback_session(&child_session_id)),
                    },
                ],
            )
            .expect("multi-result playback task should become playable");
        let library_item_id = format!("bilibili.hls.{child_session_id}");

        registry
            .complete_playback_hls_session_cached_with_metadata(
                &created.task.id,
                &child_session_id,
                library_item_id.clone(),
                completed_session.clone(),
            )
            .expect("primary child session should become completed");
        let completed = registry
            .get_task(&created.task.id)
            .expect("completed task should remain readable");

        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(
            "Generated playback",
            completed
                .playback_session
                .as_ref()
                .expect("task should keep completed playback session metadata")
                .title
        );
        assert_eq!(
            "h264-generated",
            completed
                .playback_source
                .as_ref()
                .expect("task should keep completed playback source")
                .variant_id
        );
        assert_eq!(
            i32::from(TaskState::Playable),
            completed.result_items[0].state
        );
        assert_eq!(
            i32::from(TaskState::Completed),
            completed.result_items[1].state
        );
        assert_eq!(
            Some(completed_session),
            completed.result_items[1].playback_session.clone()
        );
    }

    #[test]
    fn protects_secondary_result_sessions_after_primary_completed() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1completed-protected-secondary", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        let result_items = vec![
            BilibiliTaskResultItem {
                id: created.task.id.clone(),
                selection_id: "page:1".to_owned(),
                title: "Part 1".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-1".to_owned(),
                index: 1,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&created.task.id)),
                playback_session: Some(playback_session(&created.task.id)),
            },
            BilibiliTaskResultItem {
                id: child_session_id.clone(),
                selection_id: "page:2".to_owned(),
                title: "Part 2".to_owned(),
                subtitle: String::new(),
                source_kind: "video_page".to_owned(),
                content_id: "cid-2".to_owned(),
                index: 2,
                state: TaskState::Playable.into(),
                message: "Playable".to_owned(),
                library_item_id: String::new(),
                playback_source: Some(playback_source(&child_session_id)),
                playback_session: Some(playback_session(&child_session_id)),
            },
        ];
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                result_items,
            )
            .expect("playback results should become playable");
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &created.task.id,
                format!("bilibili.hls.{}", created.task.id),
            )
            .expect("primary session should become completed");

        let protected = registry.protected_hls_cache_session_ids();
        let playback_session_ids = registry.playback_hls_session_ids(&created.task.id);
        let primary_library_item_id = format!("bilibili.hls.{}", created.task.id);

        assert!(!protected.contains(&created.task.id));
        assert!(!protected.contains(&primary_library_item_id));
        assert!(protected.contains(&child_session_id));
        assert_eq!(
            HashSet::from([created.task.id.clone(), child_session_id]),
            playback_session_ids.into_iter().collect()
        );
    }

    #[test]
    fn playable_result_session_authorizes_serving_by_result_id() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1playable-result-session", None, None)
            .expect("playback task should be created");
        let result_session_id = "session-1";

        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "Result is playable.".to_owned(),
                playback_source(result_session_id),
                playback_session(result_session_id),
                vec![BilibiliTaskResultItem {
                    id: result_session_id.to_owned(),
                    selection_id: "page:1".to_owned(),
                    title: "Part 1".to_owned(),
                    subtitle: String::new(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-1".to_owned(),
                    index: 1,
                    state: TaskState::Playable.into(),
                    message: "Playable".to_owned(),
                    library_item_id: String::new(),
                    playback_source: Some(playback_source(result_session_id)),
                    playback_session: Some(playback_session(result_session_id)),
                }],
            )
            .expect("playback results should become playable");

        assert!(registry.is_playback_result_session_playable(result_session_id, false));
        assert!(registry.is_hls_session_playable_for_task(&created.task.id, result_session_id));
    }

    #[test]
    fn removing_completed_secondary_cache_keeps_playable_parent() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1delete-secondary-cache", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                vec![
                    BilibiliTaskResultItem {
                        id: created.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&created.task.id)),
                        playback_session: Some(playback_session(&created.task.id)),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&child_session_id)),
                        playback_session: Some(playback_session(&child_session_id)),
                    },
                ],
            )
            .expect("playback results should become playable");
        let child_library_item_id = format!("bilibili.hls.{child_session_id}");
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &child_session_id,
                child_library_item_id.clone(),
            )
            .expect("secondary session should become completed");

        let removed = registry
            .remove_completed_playback_task(&child_session_id, &child_library_item_id)
            .expect("secondary cache removal should clear result metadata");

        assert!(removed);
        let task = registry
            .get_task(&created.task.id)
            .expect("playable parent task should remain");
        assert_eq!(TaskState::Playable, task.state());
        assert_eq!(i32::from(TaskState::Playable), task.result_items[0].state);
        assert_eq!(i32::from(TaskState::Failed), task.result_items[1].state);
        assert!(task.result_items[1].library_item_id.is_empty());
        assert!(task.result_items[1].playback_source.is_none());
        assert!(task.result_items[1].playback_session.is_none());
    }

    #[test]
    fn removing_secondary_cache_updates_authoritative_output_and_resources() {
        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let state_path = temp.path().join("state").join("tasks.json");
        std::fs::create_dir_all(&root_path).unwrap();
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &state_path,
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let created = registry
            .create_bilibili_playback_task("BV1delete-v2-secondary", None, None)
            .unwrap();
        let child_session_id = format!("{}-result-2", created.task.id);
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                vec![
                    BilibiliTaskResultItem {
                        id: created.task.id.clone(),
                        state: TaskState::Playable.into(),
                        playback_source: Some(playback_source(&created.task.id)),
                        playback_session: Some(playback_session(&created.task.id)),
                        ..Default::default()
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        state: TaskState::Playable.into(),
                        playback_source: Some(playback_source(&child_session_id)),
                        playback_session: Some(playback_session(&child_session_id)),
                        ..Default::default()
                    },
                ],
            )
            .unwrap();
        let child_library_item_id = format!("bilibili.hls.{child_session_id}");
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &child_session_id,
                child_library_item_id.clone(),
            )
            .unwrap();
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "Child-Media".to_owned(),
            content_type: "video/mp4".to_owned(),
            size_bytes: 4,
            size_known: true,
            etag: "child-v1".to_owned(),
            ..Default::default()
        })
        .unwrap();
        let resource_path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(resource_path.parent().unwrap()).unwrap();
        std::fs::write(&resource_path, b"test").unwrap();
        registry
            .replace_task_output(
                &created.task.id,
                vec![
                    TaskResult {
                        id: created.task.id.clone(),
                        state: TaskState::Playable.into(),
                        playback_source: Some(playback_source(&created.task.id)),
                        ..Default::default()
                    },
                    TaskResult {
                        id: child_session_id.clone(),
                        state: TaskState::Completed.into(),
                        library_item_id: child_library_item_id.clone(),
                        playback_source: Some(playback_source(&child_session_id)),
                        artifacts: vec![TaskArtifact {
                            id: "child-media".to_owned(),
                            kind: TaskArtifactKind::Media.into(),
                            state: TaskArtifactState::Available.into(),
                            resource: Some(resource.resource.clone()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                ],
                vec![resource],
            )
            .unwrap();

        let durable_task = registry.get_task(&created.task.id).unwrap();
        let durable_output = registry.task_output_snapshot(&created.task.id).unwrap();
        let durable_state = std::fs::read(&state_path).unwrap();
        std::fs::remove_file(&state_path).unwrap();
        std::fs::create_dir(&state_path).unwrap();
        let error = registry
            .remove_completed_playback_task(&child_session_id, &child_library_item_id)
            .expect_err("playable child cache deletion must be durable");
        assert_eq!(tonic::Code::Unavailable, error.code());
        assert_eq!(durable_task, registry.get_task(&created.task.id).unwrap());
        let visible_output = registry.task_output_snapshot(&created.task.id).unwrap();
        assert_eq!(durable_output.revision, visible_output.revision);
        assert_eq!(
            durable_output.output.record.results,
            visible_output.output.record.results
        );
        assert!(registry.task_resource("child-media").is_some());
        assert!(resource_path.exists());
        std::fs::remove_dir(&state_path).unwrap();
        std::fs::write(&state_path, durable_state).unwrap();

        registry
            .remove_completed_playback_task(&child_session_id, &child_library_item_id)
            .unwrap();

        let snapshot = registry.task_output_snapshot(&created.task.id).unwrap();
        let child = snapshot
            .output
            .record
            .results
            .iter()
            .find(|result| result.id == child_session_id)
            .unwrap();
        assert_eq!(TaskState::Failed, child.state());
        assert!(child.library_item_id.is_empty());
        assert!(child.playback_source.is_none());
        assert_eq!(TaskArtifactState::Deleted, child.artifacts[0].state());
        assert!(child.artifacts[0].resource.is_none());
        assert_eq!(
            1,
            registry
                .get_task(&created.task.id)
                .unwrap()
                .output_summary
                .unwrap()
                .failed_result_count
        );
        assert!(!resource_path.exists());
        assert!(!resource_path.parent().unwrap().exists());
    }

    #[test]
    fn removing_completed_secondary_cache_keeps_completed_parent() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&state_path);
        let created = registry
            .create_bilibili_playback_task("BV1delete-completed-secondary-cache", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        registry
            .complete_playback_results_playable(
                &created.task.id,
                "Playable".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
                vec![
                    BilibiliTaskResultItem {
                        id: created.task.id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&created.task.id)),
                        playback_session: Some(playback_session(&created.task.id)),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&child_session_id)),
                        playback_session: Some(playback_session(&child_session_id)),
                    },
                ],
            )
            .expect("playback results should become playable");
        let primary_library_item_id = format!("bilibili.hls.{}", created.task.id);
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &created.task.id,
                primary_library_item_id.clone(),
            )
            .expect("primary session should become completed");
        let child_library_item_id = format!("bilibili.hls.{child_session_id}");
        registry
            .complete_playback_hls_session_cached(
                &created.task.id,
                &child_session_id,
                child_library_item_id.clone(),
            )
            .expect("secondary session should become completed");

        let durable_task = registry.get_task(&created.task.id).unwrap();
        let durable_output = registry.task_output_snapshot(&created.task.id).unwrap();
        let durable_state = std::fs::read(&state_path).unwrap();
        std::fs::remove_file(&state_path).unwrap();
        std::fs::create_dir(&state_path).unwrap();
        let error = registry
            .remove_completed_playback_task(&child_session_id, &child_library_item_id)
            .expect_err("completed child cache deletion must be durable");
        assert_eq!(tonic::Code::Unavailable, error.code());
        assert_eq!(durable_task, registry.get_task(&created.task.id).unwrap());
        let visible_output = registry.task_output_snapshot(&created.task.id).unwrap();
        assert_eq!(durable_output.revision, visible_output.revision);
        assert_eq!(
            durable_output.output.record.results,
            visible_output.output.record.results
        );
        std::fs::remove_dir(&state_path).unwrap();
        std::fs::write(&state_path, durable_state).unwrap();

        let removed = registry
            .remove_completed_playback_task(&child_session_id, &child_library_item_id)
            .expect("secondary cache removal should clear result metadata");

        assert!(removed);
        let task = registry
            .get_task(&created.task.id)
            .expect("completed parent task should remain");
        assert_eq!(TaskState::Completed, task.state());
        assert_eq!(primary_library_item_id, task.library_item_id);
        assert_eq!(i32::from(TaskState::Completed), task.result_items[0].state);
        assert_eq!(i32::from(TaskState::Failed), task.result_items[1].state);
        assert!(task.result_items[1].library_item_id.is_empty());
        assert!(task.result_items[1].playback_source.is_none());
        assert!(task.result_items[1].playback_session.is_none());
    }

    #[test]
    fn fails_unrestorable_playable_progressive_playback_tasks() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task(
                "BV1missing-manifest",
                Some(playback_options("1080p")),
                None,
            )
            .expect("playback task should be created");
        let playable = registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");

        registry.fail_unrestorable_playback_tasks(&HashSet::new(), &HashSet::new());
        let restored_task = registry
            .get_task(&playable.id)
            .expect("task should remain readable");

        assert_eq!(TaskState::Failed, restored_task.state());
        assert_eq!(
            PLAYABLE_EXPIRED_AFTER_RESTART_MESSAGE,
            restored_task.message
        );
        assert!(restored_task.playback_source.is_none());
        assert!(restored_task.playback_session.is_none());
    }

    #[test]
    fn playable_progressive_playback_task_does_not_dedupe_future_playback_requests() {
        let registry = BilibiliTaskRegistry::default();
        let options = playback_options("1080p");
        let created = registry
            .create_bilibili_playback_task("BV1playable", Some(options.clone()), None)
            .expect("playback task should be created");
        let playable = registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");

        let recreated = registry
            .create_bilibili_playback_task("BV1playable", Some(options), None)
            .expect("playable source should not block replanning");

        assert_eq!(TaskState::Playable, playable.state());
        assert!(recreated.created);
        assert_eq!(TaskState::Preparing, recreated.task.state());
        assert_ne!(playable.id, recreated.task.id);
    }

    #[test]
    fn cancelling_playable_progressive_playback_task_clears_runtime_source() {
        let registry = BilibiliTaskRegistry::default();
        let options = playback_options("1080p");
        let created = registry
            .create_bilibili_playback_task("BV1cancel-playable", Some(options.clone()), None)
            .expect("playback task should be created");
        let playable = registry
            .complete_playback_playable(
                &created.task.id,
                "Playable playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playback should become playable");

        let cancelled = registry
            .cancel_task(&playable.id)
            .expect("playable task should be cancellable");
        let stored = registry
            .get_task(&playable.id)
            .expect("cancelled task should still be readable");
        let recreated = registry
            .create_bilibili_playback_task("BV1cancel-playable", Some(options), None)
            .expect("cancelled source should be requeueable");

        assert_eq!(TaskState::Cancelled, cancelled.state());
        assert!(cancelled.playback_source.is_none());
        assert!(cancelled.playback_session.is_none());
        assert_eq!(TaskState::Cancelled, stored.state());
        assert!(stored.playback_source.is_none());
        assert!(stored.playback_session.is_none());
        assert!(recreated.created);
        assert_ne!(playable.id, recreated.task.id);
    }

    #[test]
    fn hls_finalization_after_cancel_requested_clears_result_playback_metadata() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1cancel-finalizer", None, None)
            .expect("playback task should be created");
        let primary_session_id = created.task.id.clone();
        let child_session_id = format!("{primary_session_id}-result-2");
        registry
            .complete_playback_results_playable(
                &primary_session_id,
                "Playable playback".to_owned(),
                "All results are playable.".to_owned(),
                playback_source(&primary_session_id),
                playback_session(&primary_session_id),
                vec![
                    BilibiliTaskResultItem {
                        id: primary_session_id.clone(),
                        selection_id: "page:1".to_owned(),
                        title: "Part 1".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-1".to_owned(),
                        index: 1,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&primary_session_id)),
                        playback_session: Some(playback_session(&primary_session_id)),
                    },
                    BilibiliTaskResultItem {
                        id: child_session_id.clone(),
                        selection_id: "page:2".to_owned(),
                        title: "Part 2".to_owned(),
                        subtitle: String::new(),
                        source_kind: "video_page".to_owned(),
                        content_id: "cid-2".to_owned(),
                        index: 2,
                        state: TaskState::Playable.into(),
                        message: "Playable".to_owned(),
                        library_item_id: String::new(),
                        playback_source: Some(playback_source(&child_session_id)),
                        playback_session: Some(playback_session(&child_session_id)),
                    },
                ],
            )
            .expect("playback results should become playable");
        {
            let mut inner = registry.inner.lock().expect("task registry lock poisoned");
            let task = inner
                .tasks_by_id
                .get_mut(&primary_session_id)
                .expect("task should exist");
            task.state = TaskState::CancelRequested.into();
            task.message = CANCEL_REQUESTED_MESSAGE.to_owned();
        }

        let cancelled = registry
            .complete_playback_hls_session_cached(
                &primary_session_id,
                &primary_session_id,
                format!("bilibili.hls.{primary_session_id}"),
            )
            .expect("late HLS finalization should complete cancellation");

        assert_eq!(TaskState::Cancelled, cancelled.state());
        assert_eq!(CANCELLED_BY_REQUEST_MESSAGE, cancelled.message);
        assert!(cancelled.library_item_id.is_empty());
        assert!(cancelled.playback_source.is_none());
        assert!(cancelled.playback_session.is_none());
        assert_eq!(2, cancelled.result_items.len());
        for item in &cancelled.result_items {
            assert_eq!(i32::from(TaskState::Cancelled), item.state);
            assert_eq!(CANCELLED_BY_REQUEST_MESSAGE, item.message);
            assert!(item.library_item_id.is_empty());
            assert!(item.playback_source.is_none());
            assert!(item.playback_session.is_none());
        }
    }

    #[test]
    fn restores_preparing_progressive_playback_task_as_failed() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1preparing", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        registry
            .update_playback_results(
                &created.task.id,
                Some("Partially planned playback".to_owned()),
                "Planning selected Bilibili playback results.".to_owned(),
                0.5,
                vec![BilibiliTaskResultItem {
                    id: child_session_id.clone(),
                    selection_id: "page:2".to_owned(),
                    title: "Part 2".to_owned(),
                    subtitle: String::new(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-2".to_owned(),
                    index: 2,
                    state: TaskState::Playable.into(),
                    message: "Playable".to_owned(),
                    library_item_id: String::new(),
                    playback_source: Some(playback_source(&child_session_id)),
                    playback_session: Some(playback_session(&child_session_id)),
                }],
            )
            .expect("partial playback results should persist");

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored
            .get_task(&created.task.id)
            .expect("task should restore");
        let requeued = restored
            .create_bilibili_playback_task("BV1preparing", None, None)
            .expect("failed playback source should be requeueable");

        assert_eq!(TaskState::Failed, restored_task.state());
        assert_eq!(
            PREPARING_INTERRUPTED_AFTER_RESTART_MESSAGE,
            restored_task.message
        );
        assert_eq!(1, restored_task.result_items.len());
        assert_eq!(
            i32::from(TaskState::Failed),
            restored_task.result_items[0].state
        );
        assert_eq!(
            PREPARING_INTERRUPTED_AFTER_RESTART_MESSAGE,
            restored_task.result_items[0].message
        );
        assert!(restored_task.result_items[0].library_item_id.is_empty());
        assert!(restored_task.result_items[0].playback_source.is_none());
        assert!(restored_task.result_items[0].playback_session.is_none());
        assert_ne!(created.task.id, requeued.task.id);
    }

    #[tokio::test]
    async fn restores_active_task_dedupe_by_source_and_options() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let first = registry
            .create_bilibili_task("BV1persist-dedupe", Some(download_options("720p", false)))
            .expect("task should be created");
        let second = registry
            .create_bilibili_task("BV1persist-dedupe", Some(download_options("1080p", false)))
            .expect("task with different options should be created");
        assert_ne!(first.id, second.id);

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let duplicate = restored
            .create_bilibili_task("BV1persist-dedupe", Some(download_options("720P", false)))
            .expect("duplicate active task should be returned");
        let different_subtitles = restored
            .create_bilibili_task("BV1persist-dedupe", Some(download_options("720p", true)))
            .expect("task with different options should be created");

        assert_eq!(first.id, duplicate.id);
        assert_ne!(first.id, different_subtitles.id);
    }

    #[tokio::test]
    async fn restores_running_task_as_queued_after_restart() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1running-restart", None)
            .expect("task should be created");
        let _ = registry.claim_next_bilibili_task().await;
        assert_eq!(
            TaskState::Running,
            registry
                .get_task(&task.id)
                .expect("task should exist")
                .state()
        );

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored.get_task(&task.id).expect("task should restore");
        let work_item = restored
            .try_claim_next_bilibili_task()
            .expect("requeued running task should be claimable");

        assert_eq!(TaskState::Queued, restored_task.state());
        assert_eq!(REQUEUED_AFTER_RESTART_MESSAGE, restored_task.message);
        assert_eq!(task.id, work_item.task_id);
    }

    #[tokio::test]
    async fn restores_cancel_requested_task_as_cancelled_after_restart() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1cancel-restart", None)
            .expect("task should be created");
        let _ = registry.claim_next_bilibili_task().await;

        let cancel_requested = registry.cancel_task(&task.id).expect("cancel should work");
        assert_eq!(TaskState::CancelRequested, cancel_requested.state());

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored.get_task(&task.id).expect("task should restore");
        let requeued = restored
            .create_bilibili_task("BV1cancel-restart", None)
            .expect("cancelled source should be requeueable");

        assert_eq!(TaskState::Cancelled, restored_task.state());
        assert_eq!(CANCELLED_AFTER_RESTART_MESSAGE, restored_task.message);
        assert_ne!(task.id, requeued.id);
    }

    #[test]
    fn restores_cancel_requested_progressive_playback_task_as_cancelled_and_clears_results() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1cancel-progressive-restart", None, None)
            .expect("playback task should be created");
        let child_session_id = format!("{}-result-2", created.task.id);
        registry
            .update_playback_results(
                &created.task.id,
                Some("Partially planned playback".to_owned()),
                "Planning selected Bilibili playback results.".to_owned(),
                0.5,
                vec![BilibiliTaskResultItem {
                    id: child_session_id.clone(),
                    selection_id: "page:2".to_owned(),
                    title: "Part 2".to_owned(),
                    subtitle: String::new(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-2".to_owned(),
                    index: 2,
                    state: TaskState::Playable.into(),
                    message: "Playable".to_owned(),
                    library_item_id: String::new(),
                    playback_source: Some(playback_source(&child_session_id)),
                    playback_session: Some(playback_session(&child_session_id)),
                }],
            )
            .expect("partial playback results should persist");

        let cancel_requested = registry
            .cancel_task(&created.task.id)
            .expect("playback cancel should work");
        assert_eq!(TaskState::CancelRequested, cancel_requested.state());

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored
            .get_task(&created.task.id)
            .expect("task should restore");
        let requeued = restored
            .create_bilibili_playback_task("BV1cancel-progressive-restart", None, None)
            .expect("cancelled playback source should be requeueable");

        assert_eq!(TaskState::Cancelled, restored_task.state());
        assert_eq!(CANCELLED_AFTER_RESTART_MESSAGE, restored_task.message);
        assert_eq!(1, restored_task.result_items.len());
        assert_eq!(
            i32::from(TaskState::Cancelled),
            restored_task.result_items[0].state
        );
        assert_eq!(
            HashSet::from([child_session_id]),
            restored.interrupted_planning_result_session_ids()
        );
        assert!(restored_task.result_items[0].library_item_id.is_empty());
        assert!(restored_task.result_items[0].playback_source.is_none());
        assert!(restored_task.result_items[0].playback_session.is_none());
        assert_ne!(created.task.id, requeued.task.id);
    }

    #[test]
    fn invalid_persisted_state_disables_writeback_without_overwriting_file() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let root_path = temp.path().join("cache");
        let resource_path = root_path.join(".tvos-net-player/resources/unknown-owner/body");
        std::fs::create_dir_all(resource_path.parent().unwrap()).unwrap();
        std::fs::write(&resource_path, b"preserve").unwrap();
        TaskStateStore::new(&path)
            .save(&[persisted_task_record("repaired-task", "BV1repaired-state")])
            .expect("repair snapshot should be written");
        let repair_snapshot = std::fs::read(&path).expect("repair snapshot should be readable");
        std::fs::write(&path, b"{ invalid json").expect("invalid state should be written");

        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &path,
            TaskRetentionPolicy::default(),
            Some(root_path),
        );
        let task = registry
            .create_bilibili_task("BV1invalid-state", None)
            .expect("registry should remain usable in memory");
        let persisted = std::fs::read_to_string(&path).expect("state file should remain readable");

        assert_eq!(TaskState::Queued, task.state());
        assert!(registry.persistence_configured());
        assert!(!registry.persistence_available());
        assert!(!registry.retry_pending_persistence());
        assert_eq!("{ invalid json", persisted);
        assert_eq!(
            b"{ invalid json",
            std::fs::read(&path)
                .expect("a persistence retry must leave the malformed snapshot unchanged")
                .as_slice()
        );
        assert!(resource_path.exists());

        let volatile_task_id = task.id;
        drop(registry);
        std::fs::write(&path, repair_snapshot).expect("task snapshot should be repaired");
        let recovered = BilibiliTaskRegistry::with_persistence_path(&path);

        assert!(recovered.persistence_configured());
        assert!(recovered.persistence_available());
        assert!(recovered.get_task("repaired-task").is_ok());
        assert_eq!(
            tonic::Code::NotFound,
            recovered.get_task(&volatile_task_id).unwrap_err().code()
        );
    }

    #[test]
    fn post_load_save_failure_disables_and_successful_retry_restores_persistence() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        assert!(registry.persistence_available());
        let task = registry
            .create_bilibili_task("BV1failed-write", None)
            .expect("initial task creation should persist");

        std::fs::remove_file(&path).expect("initial persistence probe should create a file");
        std::fs::create_dir(&path).expect("directory should block atomic snapshot replacement");
        let results = vec![TaskResult {
            id: "result-one".to_owned(),
            state: TaskState::Completed.into(),
            ..Default::default()
        }];
        let error = registry
            .replace_task_output(&task.id, results.clone(), Vec::new())
            .expect_err("authoritative output mutation must report failed durability");
        assert_eq!(tonic::Code::Unavailable, error.code());

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        registry
            .replace_task_output(&task.id, results, Vec::new())
            .expect("an identical retry should persist the installed output");
        assert!(registry.persistence_available());
        drop(registry);
        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        assert_eq!(
            "result-one",
            restored
                .task_output_snapshot(&task.id)
                .unwrap()
                .output
                .record
                .results[0]
                .id
        );
    }

    #[test]
    fn rejected_task_creation_rolls_back_queue_and_allows_retry() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        std::fs::remove_file(&path).expect("initial persistence probe should create a file");
        std::fs::create_dir(&path).expect("directory should block atomic snapshot replacement");

        let download_error = registry
            .create_bilibili_task("BV1rejected-create", None)
            .expect_err("download task creation must require a committed snapshot");
        let playback_error =
            match registry.create_bilibili_playback_task("BV1rejected-playback", None, None) {
                Ok(_) => panic!("playback task creation must require a committed snapshot"),
                Err(error) => error,
            };

        assert_eq!(tonic::Code::Unavailable, download_error.code());
        assert_eq!(tonic::Code::Unavailable, playback_error.code());
        assert!(registry.try_claim_next_bilibili_task().is_none());

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        let download = registry
            .create_bilibili_task("BV1rejected-create", None)
            .expect("download task creation should retry after repair");
        let playback = registry
            .create_bilibili_playback_task("BV1rejected-playback", None, None)
            .expect("playback task creation should retry after repair");

        assert_eq!(
            download.id,
            registry.try_claim_next_bilibili_task().unwrap().task_id
        );
        assert_eq!(
            playback.task.id,
            registry.get_task(&playback.task.id).unwrap().id
        );
    }

    #[test]
    fn installed_snapshot_stays_visible_when_directory_sync_fails() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1post-rename", None)
            .expect("task should be created durably");
        let results = vec![TaskResult {
            id: "installed-result".to_owned(),
            state: TaskState::Completed.into(),
            ..Default::default()
        }];

        registry.fail_next_persistence_directory_sync();
        let updated = registry
            .replace_task_output(&task.id, results.clone(), Vec::new())
            .expect("an installed snapshot must remain accepted");

        assert!(!registry.persistence_available());
        assert_eq!(
            updated.output_summary,
            registry.get_task(&task.id).unwrap().output_summary
        );
        let installed_records = TaskStateStore::new(&path)
            .load()
            .expect("the renamed snapshot should be readable");
        assert_eq!(1, installed_records.len());
        assert_eq!(results, installed_records[0].output.results);

        registry
            .replace_task_output(&task.id, results, Vec::new())
            .expect("an identical retry should restore durable persistence");
        assert!(registry.persistence_available());
    }

    #[test]
    fn hls_completion_remains_retryable_until_directory_sync_is_durable() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1hls-completion-directory-sync", None, None)
            .expect("playback task should be created durably");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playable state should persist");
        let library_item_id = format!("bilibili.hls.{}", created.task.id);

        registry.fail_next_persistence_directory_sync();
        let error = registry
            .complete_playback_cached(&created.task.id, library_item_id.clone())
            .expect_err("the finalizer owner must remain queued until directory sync succeeds");

        assert_eq!(tonic::Code::Unavailable, error.code());
        assert!(!registry.persistence_available());
        let installed = registry
            .get_task(&created.task.id)
            .expect("the installed completion should remain visible while durability retries");
        assert_eq!(TaskState::Completed, installed.state());
        assert_eq!(library_item_id, installed.library_item_id);

        registry.fail_next_persistence_directory_sync();
        let error = registry
            .complete_playback_cached(&created.task.id, library_item_id.clone())
            .expect_err("another directory sync failure must keep the finalizer queued");
        assert_eq!(tonic::Code::Unavailable, error.code());
        assert!(!registry.persistence_available());

        let completed = registry
            .complete_playback_cached(&created.task.id, library_item_id.clone())
            .expect("the finalizer retry should make the installed completion durable");
        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(library_item_id, completed.library_item_id);
        assert!(registry.persistence_available());
    }

    #[test]
    fn cache_fill_failure_remains_pending_until_persistence_recovers() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1cache-fill-pending", None, None)
            .expect("playback task should be created durably");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playable state should persist");

        std::fs::remove_file(&path).expect("state file should be removable");
        std::fs::create_dir(&path).expect("directory should block snapshot replacement");
        let error = registry
            .fail_hls_cache_fill_for_playback_session(
                &created.task.id,
                &created.task.id,
                "Playable online; offline cache fill failed.".to_owned(),
            )
            .expect_err("an unpersisted cache-fill failure must remain pending");

        assert_eq!(tonic::Code::Unavailable, error.code());
        assert!(!registry.persistence_available());
        assert_eq!(
            PLAYBACK_PLAYABLE_MESSAGE,
            registry.get_task(&created.task.id).unwrap().message
        );

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        assert!(registry.retry_pending_persistence());
        let persisted = registry
            .get_task(&created.task.id)
            .expect("task should remain readable");
        assert_eq!(
            "Playable online; offline cache fill failed.",
            persisted.message
        );
        assert!(registry.persistence_available());
    }

    #[test]
    fn restored_session_failure_rolls_back_until_directory_sync_is_durable() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1restore-failure-pending", None, None)
            .expect("playback task should be created durably");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Playable".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playable state should persist");

        registry.fail_next_persistence_directory_sync();
        let error = registry
            .fail_unrestorable_playback_session_after_cache_restore(
                &created.task.id,
                "Restored session failed.".to_owned(),
            )
            .expect_err("directory durability is required before deleting restored media");

        assert_eq!(tonic::Code::Unavailable, error.code());
        assert!(!registry.persistence_available());
        assert_eq!(
            TaskState::Playable,
            registry.get_task(&created.task.id).unwrap().state()
        );
        assert!(registry.is_primary_hls_session_playable(&created.task.id, &created.task.id));

        assert!(registry.retry_pending_persistence());
        let failed = registry
            .fail_unrestorable_playback_session_after_cache_restore(
                &created.task.id,
                "Restored session failed.".to_owned(),
            )
            .expect("durable retry should succeed")
            .expect("restored session should still be referenced");
        assert_eq!(TaskState::Failed, failed.state());
        assert!(registry.persistence_available());
    }

    #[test]
    fn failed_snapshot_keeps_committed_hls_authorization_until_cancellation_can_persist() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1committed-hls", None, None)
            .expect("playback task should be created durably");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Committed playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("playable state should be persisted");

        std::fs::remove_file(&path).expect("state file should be removable");
        std::fs::create_dir(&path).expect("directory should block snapshot replacement");
        let completion_error = registry
            .complete_task_failed(&created.task.id, "Hidden failure.".to_owned())
            .expect_err("unpersisted terminal state must not be acknowledged");

        assert_eq!(tonic::Code::Unavailable, completion_error.code());
        assert!(!registry.persistence_available());
        assert_eq!(
            TaskState::Playable,
            registry.get_task(&created.task.id).unwrap().state()
        );
        assert!(registry.is_playback_task_playable(&created.task.id));
        assert!(registry.is_primary_hls_session_playable(&created.task.id, &created.task.id));
        assert!(registry.is_hls_session_playable_for_task(&created.task.id, &created.task.id));
        assert_eq!(
            Some(created.task.id.clone()),
            registry.playable_task_id_for_hls_session(&created.task.id)
        );
        assert_eq!(
            vec![created.task.id.clone()],
            registry.playback_hls_session_ids(&created.task.id)
        );
        assert!(
            registry
                .protected_hls_cache_session_ids()
                .contains(&created.task.id),
            "eviction protection must follow the last committed playable task"
        );

        let error = registry
            .cancel_task(&created.task.id)
            .expect_err("cancellation must not act on an uncommitted terminal revision");
        assert_eq!(tonic::Code::Unavailable, error.code());
        assert_eq!(
            TaskState::Playable,
            registry.get_task(&created.task.id).unwrap().state()
        );
        assert!(registry.is_playback_task_playable(&created.task.id));

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        let completed = registry
            .complete_task_failed(&created.task.id, "Hidden failure.".to_owned())
            .expect("terminal completion retry should publish pending state");
        assert_eq!(TaskState::Failed, completed.state());
        assert_eq!(
            TaskState::Failed,
            registry.get_task(&created.task.id).unwrap().state()
        );
        assert!(registry.persistence_available());
    }

    #[test]
    fn running_cancellation_is_acknowledged_only_after_persistence_recovers() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1cancel-persistence", None)
            .expect("task should be created durably");
        let work_item = registry
            .try_claim_next_bilibili_task()
            .expect("task should start running");

        std::fs::remove_file(&path).expect("state file should be removable");
        std::fs::create_dir(&path).expect("directory should block snapshot replacement");
        let error = registry
            .cancel_task(&task.id)
            .expect_err("unpersisted cancellation must not be acknowledged");

        assert_eq!(tonic::Code::Unavailable, error.code());
        assert!(work_item.cancellation.is_cancel_requested());
        assert_eq!(
            TaskState::Running,
            registry.get_task(&task.id).unwrap().state()
        );

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        let cancelled = registry
            .cancel_task(&task.id)
            .expect("cancellation retry should persist pending intent");
        assert_eq!(TaskState::CancelRequested, cancelled.state());
        assert_eq!(
            TaskState::CancelRequested,
            registry.get_task(&task.id).unwrap().state()
        );
        assert!(registry.persistence_available());
    }

    #[test]
    fn cancellation_captures_hls_sessions_after_pending_playable_repair() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let created = registry
            .create_bilibili_playback_task("BV1pending-playable-cancel", None, None)
            .expect("playback task should be created durably");

        std::fs::remove_file(&path).expect("state file should be removable");
        std::fs::create_dir(&path).expect("directory should block snapshot replacement");
        registry
            .complete_playback_playable(
                &created.task.id,
                "Pending playback".to_owned(),
                playback_source(&created.task.id),
                playback_session(&created.task.id),
            )
            .expect("legacy playable mutation should remain staged in memory");

        assert_eq!(
            TaskState::Preparing,
            registry.get_task(&created.task.id).unwrap().state()
        );
        assert!(
            registry
                .playback_hls_session_ids(&created.task.id)
                .is_empty()
        );
        assert_eq!(
            HlsSessionPublicationState::Pending,
            registry.hls_session_publication_state(&created.task.id, &created.task.id)
        );

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        let cancellation = registry
            .cancel_task_with_hls_session_ids(&created.task.id)
            .expect("cancellation should repair and then persist pending playback");

        assert_eq!(TaskState::Cancelled, cancellation.task.state());
        assert_eq!(vec![created.task.id], cancellation.hls_session_ids);
        assert!(registry.persistence_available());
    }

    #[test]
    fn task_reads_remain_available_while_snapshot_io_is_blocked() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = Arc::new(BilibiliTaskRegistry::with_persistence_path(&path));
        let task = registry
            .create_bilibili_task("BV1unlocked-save", None)
            .expect("task should be created durably");
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        registry.block_next_persistence_save(Arc::clone(&entered), Arc::clone(&resume));

        let writer_registry = Arc::clone(&registry);
        let task_id = task.id.clone();
        let writer = std::thread::spawn(move || {
            writer_registry.complete_task_failed(&task_id, "Expected failure.".to_owned())
        });
        entered.wait();

        let reader_registry = Arc::clone(&registry);
        let reader_task_id = task.id.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            sender
                .send(reader_registry.get_task(&reader_task_id))
                .expect("reader result should be delivered");
        });
        let read_while_blocked = receiver.recv_timeout(Duration::from_millis(250));
        resume.wait();

        let visible = read_while_blocked
            .expect("task reads must not wait for snapshot I/O")
            .expect("the committed task should remain readable");
        assert_eq!(TaskState::Queued, visible.state());
        reader.join().expect("reader thread should finish");
        let completed = writer
            .join()
            .expect("writer thread should finish")
            .expect("legacy mutation should complete");
        assert_eq!(TaskState::Failed, completed.state());
        assert_eq!(
            TaskState::Failed,
            registry.get_task(&task.id).unwrap().state()
        );
    }

    #[tokio::test]
    async fn legacy_output_is_published_only_after_failed_persistence_recovers() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1durable-watch", None)
            .expect("task should be created durably");
        let mut subscription = registry
            .subscribe(std::slice::from_ref(&task.id))
            .expect("watcher should subscribe");
        let durable_task = registry.get_task(&task.id).unwrap();

        std::fs::remove_file(&path).expect("state file should be removable");
        std::fs::create_dir(&path).expect("directory should block snapshot replacement");
        let error = registry
            .complete_task_failed(&task.id, "Expected failure.".to_owned())
            .expect_err("an unpersisted terminal state must not be acknowledged");
        assert_eq!(tonic::Code::Unavailable, error.code());
        assert!(!registry.persistence_available());
        assert_eq!(durable_task, registry.get_task(&task.id).unwrap());
        let mut reconnected = registry
            .subscribe(std::slice::from_ref(&task.id))
            .expect("reconnected watcher should subscribe to the committed view");
        assert_eq!(vec![durable_task], reconnected.snapshots());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), subscription.recv())
                .await
                .is_err(),
            "an unpersisted legacy output revision must not be published"
        );

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        registry
            .create_bilibili_task("BV1durability-recovery", None)
            .expect("an unrelated mutation should retry the full snapshot");
        let published = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("durable recovery should publish pending tasks")
            .expect("watcher should remain active");
        assert_eq!(TaskState::Failed, published.state());
        let reconnected_event = tokio::time::timeout(Duration::from_secs(1), reconnected.recv())
            .await
            .expect("reconnected watcher should receive durable recovery")
            .expect("reconnected watcher should remain active");
        assert_eq!(published, reconnected_event);
        assert_eq!(
            registry
                .get_task(&task.id)
                .unwrap()
                .output_summary
                .as_ref()
                .map(|summary| summary.revision),
            published
                .output_summary
                .as_ref()
                .map(|summary| summary.revision)
        );

        drop(subscription);
        drop(registry);
        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        assert_eq!(
            TaskState::Failed,
            restored.get_task(&task.id).unwrap().state()
        );
    }

    #[test]
    fn invalid_generic_output_cleans_new_staged_resource_bodies() {
        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &path,
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1invalid-generic-output", None)
            .expect("task should be created durably");
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "invalid-output-resource".to_owned(),
            content_type: "text/plain".to_owned(),
            size_bytes: 4,
            size_known: true,
            ..Default::default()
        })
        .unwrap();
        let resource_path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(resource_path.parent().unwrap())
            .expect("staged resource directory should be created");
        std::fs::write(&resource_path, b"test").expect("staged body should be written");
        let invalid_results = vec![TaskResult {
            id: String::new(),
            state: TaskState::Completed.into(),
            artifacts: vec![TaskArtifact {
                id: "artifact-one".to_owned(),
                kind: TaskArtifactKind::Metadata.into(),
                state: TaskArtifactState::Available.into(),
                resource: Some(resource.resource.clone()),
                ..Default::default()
            }],
            ..Default::default()
        }];

        let error = registry
            .replace_task_output(&task.id, invalid_results, vec![resource])
            .expect_err("invalid task output must be rejected");

        assert_eq!(tonic::Code::InvalidArgument, error.code());
        assert!(!resource_path.exists());
        assert!(!resource_path.parent().unwrap().exists());
        assert!(registry.task_resource("invalid-output-resource").is_none());
    }

    #[test]
    fn missing_task_output_rejects_before_resource_body_creation() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let first = test_task_resource("missing-task-first", 5);
        let second = test_task_resource("missing-task-second", 6);
        let first_path = root_path.join(first.relative_path());
        let second_path = root_path.join(second.relative_path());

        let error =
            match registry.stage_task_output_replacement("missing-task", vec![first, second]) {
                Ok(_) => panic!("output for a missing task must be rejected before body creation"),
                Err(error) => error,
            };

        assert_eq!(tonic::Code::NotFound, error.code());
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[test]
    fn dropping_staged_output_before_body_creation_keeps_v2_available() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1drop-before-body", None)
            .expect("task should be created");
        let resource = test_task_resource("drop-before-body-resource", 4);
        let body_path = root_path.join(resource.relative_path());

        {
            let staged = registry
                .stage_task_output_replacement(&task.id, vec![resource.clone()])
                .expect("new resource should acquire staged ownership");
            assert_eq!(1, staged.resources_requiring_body_creation().count());
        }

        assert!(registry.task_output_v2_available());
        assert!(!body_path.exists());
        assert!(!body_path.parent().unwrap().exists());

        let retry = registry
            .stage_task_output_replacement(&task.id, vec![resource])
            .expect("a never-created body should remain claimable");
        assert_eq!(1, retry.resources_requiring_body_creation().count());
        drop(retry);
        assert!(registry.task_output_v2_available());
    }

    #[test]
    fn resource_id_collision_rejects_before_body_creation_and_preserves_live_body() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let owner = registry
            .create_bilibili_task("BV1resource-owner", None)
            .expect("resource owner should be created");
        let rejected = registry
            .create_bilibili_task("BV1resource-collision", None)
            .expect("rejected output task should be created");
        let live = test_task_resource("live-resource", 4);
        let staged_live = registry
            .stage_task_output_replacement(&owner.id, vec![live.clone()])
            .expect("live resource should acquire staged ownership");
        let live_path = write_task_resource_body(&root_path, &live, b"live");
        staged_live
            .commit(vec![test_task_result_with_resources(
                "owner-result",
                std::slice::from_ref(&live),
            )])
            .expect("live resource should commit");
        let first = test_task_resource("collision-first", 5);
        let second = test_task_resource("collision-second", 6);
        let first_path = root_path.join(first.relative_path());
        let second_path = root_path.join(second.relative_path());

        let error = match registry
            .stage_task_output_replacement(&rejected.id, vec![live.clone(), first, second])
        {
            Ok(_) => panic!("a live resource id must reject staging before body creation"),
            Err(error) => error,
        };

        assert_eq!(tonic::Code::AlreadyExists, error.code());
        assert_eq!(b"live", std::fs::read(&live_path).unwrap().as_slice());
        assert!(registry.task_resource(&live.resource.id).is_some());
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[test]
    fn existing_task_resource_is_not_exposed_for_body_recreation() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1immutable-resource", None)
            .expect("task should be created");
        let resource = test_task_resource("immutable-resource", 4);
        let initial = registry
            .stage_task_output_replacement(&task.id, vec![resource.clone()])
            .expect("new resource should acquire staged ownership");
        let initial_resource = initial
            .resources_requiring_body_creation()
            .next()
            .expect("new resource should require body creation")
            .clone();
        let body_path = write_task_resource_body(&root_path, &initial_resource, b"live");
        let results = vec![test_task_result_with_resources(
            "immutable-result",
            std::slice::from_ref(&resource),
        )];
        initial
            .commit(results.clone())
            .expect("initial resource should commit");

        let retry = registry
            .stage_task_output_replacement(&task.id, vec![resource.clone()])
            .expect("identical resource metadata should be reusable");
        assert_eq!(0, retry.resources_requiring_body_creation().count());
        retry
            .commit(results)
            .expect("identical output should remain durable");
        assert_eq!(b"live", std::fs::read(&body_path).unwrap().as_slice());

        let duplicate_error = match registry
            .stage_task_output_replacement(&task.id, vec![resource.clone(), resource.clone()])
        {
            Ok(_) => panic!("duplicate resource ids must fail before body creation"),
            Err(error) => error,
        };
        assert_eq!(tonic::Code::InvalidArgument, duplicate_error.code());
        assert_eq!(b"live", std::fs::read(&body_path).unwrap().as_slice());

        let mut rebound = resource;
        rebound.resource.etag = "different-representation".to_owned();
        let error = match registry.stage_task_output_replacement(&task.id, vec![rebound]) {
            Ok(_) => panic!("resource representation changes must fail before body creation"),
            Err(error) => error,
        };
        assert_eq!(tonic::Code::InvalidArgument, error.code());
        assert_eq!(b"live", std::fs::read(&body_path).unwrap().as_slice());
    }

    #[test]
    fn expired_task_resource_is_retired_durably_and_its_body_is_removed() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("state").join("tasks.json");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &state_path,
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1expired-resource", None)
            .expect("task should be created");
        let mut resource = test_task_resource("expired-output-resource", 7);
        resource.resource.expires_at = Some(Timestamp {
            seconds: 0,
            nanos: 0,
        });
        let body_path = write_task_resource_body(&root_path, &resource, b"expired");
        registry
            .replace_task_output(
                &task.id,
                vec![test_task_result_with_resources(
                    "expired-result",
                    std::slice::from_ref(&resource),
                )],
                vec![resource],
            )
            .expect("expired resource metadata should be accepted before retirement");
        assert!(body_path.exists());

        let snapshot = registry
            .retain_task_output_snapshot(&task.id, Instant::now() + Duration::from_secs(60))
            .expect("reading task output should retire expired resources");

        assert!(snapshot.output.record.resources.is_empty());
        let artifact = &snapshot.output.record.results[0].artifacts[0];
        assert_eq!(TaskArtifactState::Unavailable, artifact.state());
        assert!(artifact.resource.is_none());
        assert_eq!(
            "cache.resource_expired",
            artifact.problem.as_ref().unwrap().code
        );
        assert_eq!(
            0,
            registry
                .get_task(&task.id)
                .unwrap()
                .output_summary
                .unwrap()
                .available_artifact_count
        );
        assert!(!body_path.exists());
        assert!(!body_path.parent().unwrap().exists());

        drop(registry);
        let restored = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &state_path,
            TaskRetentionPolicy::default(),
            Some(root_path),
        );
        let restored_output = restored.task_output_snapshot(&task.id).unwrap();
        assert!(restored_output.output.record.resources.is_empty());
        assert_eq!(
            TaskArtifactState::Unavailable,
            restored_output.output.record.results[0].artifacts[0].state()
        );
    }

    #[tokio::test]
    async fn failed_generic_output_is_rolled_back_until_an_explicit_retry_succeeds() {
        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &path,
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1generic-watch", None)
            .expect("task should be created durably");
        let mut subscription = registry
            .subscribe(std::slice::from_ref(&task.id))
            .expect("watcher should subscribe");
        let durable_task = registry.get_task(&task.id).unwrap();
        let durable_output = registry.task_output_snapshot(&task.id).unwrap();
        let durable_state = std::fs::read(&path).expect("durable state should be readable");
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "failed-output-resource".to_owned(),
            content_type: "text/plain".to_owned(),
            size_bytes: 4,
            size_known: true,
            etag: "failed-v1".to_owned(),
            ..Default::default()
        })
        .unwrap();
        let resource_path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(resource_path.parent().unwrap())
            .expect("staged resource directory should be created");
        std::fs::write(&resource_path, b"old!").expect("staged resource body should be written");
        let results = vec![TaskResult {
            id: "result-one".to_owned(),
            state: TaskState::Completed.into(),
            artifacts: vec![TaskArtifact {
                id: "artifact-one".to_owned(),
                kind: TaskArtifactKind::Metadata.into(),
                state: TaskArtifactState::Available.into(),
                resource: Some(resource.resource.clone()),
                ..Default::default()
            }],
            ..Default::default()
        }];

        std::fs::remove_file(&path).expect("state file should be removable");
        std::fs::create_dir(&path).expect("directory should block snapshot replacement");
        let error = registry
            .replace_task_output(&task.id, results.clone(), vec![resource.clone()])
            .expect_err("authoritative output must report failed durability");
        assert_eq!(tonic::Code::Unavailable, error.code());
        assert_eq!(durable_task, registry.get_task(&task.id).unwrap());
        let visible_output = registry.task_output_snapshot(&task.id).unwrap();
        assert_eq!(durable_output.revision, visible_output.revision);
        assert_eq!(durable_output.snapshot_id, visible_output.snapshot_id);
        assert_eq!(
            durable_output.output.record.results,
            visible_output.output.record.results
        );
        assert!(registry.task_resource("failed-output-resource").is_none());
        assert!(!resource_path.exists());
        assert!(!resource_path.parent().unwrap().exists());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), subscription.recv())
                .await
                .is_err(),
            "failed authoritative output must remain unpublished"
        );
        let mut reconnected = registry
            .subscribe(std::slice::from_ref(&task.id))
            .expect("a new watcher should subscribe to the durable view");
        assert_eq!(vec![durable_task.clone()], reconnected.snapshots());

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        std::fs::write(&path, durable_state).expect("durable state should be restored");
        let restarted = BilibiliTaskRegistry::with_persistence_path(&path);
        let restarted_output = restarted.task_output_snapshot(&task.id).unwrap();
        assert_eq!(durable_output.revision, restarted_output.revision);
        assert_eq!(
            durable_output.output.record.results,
            restarted_output.output.record.results
        );
        assert!(restarted.task_resource("failed-output-resource").is_none());
        drop(restarted);

        registry
            .create_bilibili_task("BV1generic-recovery", None)
            .expect("an unrelated mutation should restore persistence health");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), subscription.recv())
                .await
                .is_err(),
            "an unrelated recovery must not publish the rolled-back output"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(25), reconnected.recv())
                .await
                .is_err(),
            "a reconnected watcher must not receive the rolled-back output"
        );

        std::fs::create_dir_all(resource_path.parent().unwrap())
            .expect("retry resource directory should be created");
        std::fs::write(&resource_path, b"new!").expect("retry resource body should be written");
        registry
            .replace_task_output(&task.id, results.clone(), vec![resource.clone()])
            .expect("an explicit retry should persist the authoritative output");
        let published = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("durable retry should publish output")
            .expect("watcher should remain active");
        let durable_revision = registry.task_output_snapshot(&task.id).unwrap().revision;
        assert_eq!(
            Some(durable_revision),
            published
                .output_summary
                .as_ref()
                .map(|summary| summary.revision)
        );
        let reconnected_event = tokio::time::timeout(Duration::from_secs(1), reconnected.recv())
            .await
            .expect("durable retry should publish to reconnected watcher")
            .expect("reconnected watcher should remain active");
        assert_eq!(published.output_summary, reconnected_event.output_summary);
        assert!(registry.task_resource("failed-output-resource").is_some());

        registry
            .replace_task_output(&task.id, results, vec![resource])
            .expect("identical output should already be durable");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), subscription.recv())
                .await
                .is_err(),
            "an identical durable retry must not publish a duplicate event"
        );
    }

    #[test]
    fn direct_restore_rejects_normalized_duplicate_task_ids() {
        let first = persisted_task_record("normalized-identity", "BV1normalized-one");
        let mut duplicate = persisted_task_record("distinct-identity", "BV1normalized-two");
        duplicate.task.id = format!("\u{2003}{}\u{2003}", first.task.id);

        let error = match BilibiliTaskRegistry::from_persisted_records(
            vec![first, duplicate],
            None,
            true,
            TaskRetentionPolicy::default(),
            None,
        ) {
            Ok(_) => panic!("direct restore must reject normalized duplicate task ids"),
            Err(error) => error,
        };

        assert_eq!(io::ErrorKind::InvalidData, error.kind());
    }

    #[test]
    fn colliding_task_identity_blocks_startup_rewrite_and_resource_cleanup() {
        for (fixture_name, duplicate_task_id) in [
            ("duplicate-task-id", true),
            ("duplicate-snapshot-id", false),
        ] {
            let temp = tempfile::tempdir().expect("temp dir should be created");
            let state_path = temp.path().join("state").join("tasks.json");
            let root_path = temp.path().join("cache");
            let first_resource = test_task_resource("collision-owned-one", 3);
            let second_resource = test_task_resource("collision-owned-two", 3);
            let first_path = write_task_resource_body(&root_path, &first_resource, b"one");
            let second_path = write_task_resource_body(&root_path, &second_resource, b"two");
            let record = |id: &str, source: &str, resource: TaskResourceRecord| {
                let mut record = persisted_task_record(id, source);
                record.output = TaskOutputRecord::replace(
                    Some(&record.output),
                    vec![test_task_result_with_resources(
                        &format!("result-{id}"),
                        std::slice::from_ref(&resource),
                    )],
                    vec![resource],
                )
                .expect("persisted output should be valid");
                record
            };
            TaskStateStore::new(&state_path)
                .save(&[
                    record("collision-one", "BV1collision-one", first_resource),
                    record("collision-two", "BV1collision-two", second_resource),
                ])
                .expect("valid fixture should persist");
            let mut snapshot: serde_json::Value = serde_json::from_slice(
                &std::fs::read(&state_path).expect("fixture should be readable"),
            )
            .expect("fixture should decode");
            if duplicate_task_id {
                let first_id = snapshot["tasks"][0]["id"]
                    .as_str()
                    .expect("fixture task id should be a string");
                snapshot["tasks"][1]["id"] =
                    serde_json::Value::String(format!("\u{2003}{first_id}\u{2003}"));
            } else {
                snapshot["tasks"][1]["output"]["snapshot_id"] =
                    snapshot["tasks"][0]["output"]["snapshot_id"].clone();
            }
            let mut colliding_bytes =
                serde_json::to_vec_pretty(&snapshot).expect("colliding fixture should serialize");
            colliding_bytes.push(b'\n');
            std::fs::write(&state_path, &colliding_bytes)
                .expect("colliding fixture should be written");

            let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
                &state_path,
                TaskRetentionPolicy::default(),
                Some(root_path),
            );

            assert!(
                !registry.persistence_available(),
                "{fixture_name} must disable persistence"
            );
            assert!(
                !registry.task_output_v2_available(),
                "{fixture_name} must disable task output v2"
            );
            assert_eq!(
                colliding_bytes,
                std::fs::read(&state_path).expect("colliding snapshot should remain untouched")
            );
            assert!(first_path.exists());
            assert!(second_path.exists());
        }
    }

    #[test]
    fn failed_startup_rewrite_preserves_resources_from_skipped_records() {
        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("state").join("tasks.json");
        let root_path = temp.path().join("cache");
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "skipped-record-resource".to_owned(),
            content_type: "text/vtt".to_owned(),
            size_bytes: 4,
            size_known: true,
            etag: "v1".to_owned(),
            ..Default::default()
        })
        .expect("resource should be valid");
        let resource_path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(resource_path.parent().unwrap()).unwrap();
        std::fs::write(&resource_path, b"test").unwrap();

        let mut record = persisted_task_record("invalid-task", "");
        record.output = TaskOutputRecord::replace(
            Some(&record.output),
            vec![TaskResult {
                id: "result-one".to_owned(),
                state: TaskState::Completed.into(),
                artifacts: vec![TaskArtifact {
                    id: "subtitle-one".to_owned(),
                    kind: TaskArtifactKind::Subtitle.into(),
                    state: TaskArtifactState::Available.into(),
                    resource: Some(resource.resource.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            vec![resource],
        )
        .expect("persisted output should be valid");
        TaskStateStore::new(&state_path)
            .save(&[record])
            .expect("fixture should persist");
        std::fs::create_dir(state_path.with_file_name("tasks.json.tmp"))
            .expect("directory should block startup rewrite");

        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &state_path,
            TaskRetentionPolicy::default(),
            Some(root_path),
        );

        assert!(!registry.persistence_available());
        assert!(!registry.task_output_v2_available());
        assert!(resource_path.exists());

        std::fs::remove_dir(state_path.with_file_name("tasks.json.tmp"))
            .expect("startup rewrite blocker should be removable");
        registry
            .create_bilibili_task("BV1resource-scan-recovery", None)
            .expect("a durable mutation should retry the orphan scan");

        assert!(registry.persistence_available());
        assert!(registry.task_output_v2_available());
        assert!(!resource_path.exists());
        assert!(!resource_path.parent().unwrap().exists());
    }

    #[test]
    fn startup_scan_preserves_owned_resources_until_their_transition_is_durable() {
        use crate::generated::tvos_net_player::v1::{
            TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("state").join("tasks.json");
        let root_path = temp.path().join("cache");
        let pending = test_task_resource("pending-owned-resource", 7);
        let unavailable = test_task_resource("unavailable-owned-resource", 11);
        let mut expired = test_task_resource("expired-owned-resource", 7);
        expired.resource.expires_at = Some(Timestamp {
            seconds: 0,
            nanos: 0,
        });
        let pending_path = write_task_resource_body(&root_path, &pending, b"pending");
        let unavailable_path = write_task_resource_body(&root_path, &unavailable, b"unavailable");
        let expired_path = write_task_resource_body(&root_path, &expired, b"expired");

        let mut record = persisted_task_record("owned-resource-task", "BV1owned-resource");
        record.output = TaskOutputRecord::replace(
            Some(&record.output),
            vec![TaskResult {
                id: "owned-resource-result".to_owned(),
                state: TaskState::Completed.into(),
                artifacts: vec![
                    TaskArtifact {
                        id: "pending-artifact".to_owned(),
                        kind: TaskArtifactKind::Subtitle.into(),
                        state: TaskArtifactState::Pending.into(),
                        resource: Some(pending.resource.clone()),
                        ..Default::default()
                    },
                    TaskArtifact {
                        id: "unavailable-artifact".to_owned(),
                        kind: TaskArtifactKind::Subtitle.into(),
                        state: TaskArtifactState::Unavailable.into(),
                        resource: Some(unavailable.resource.clone()),
                        ..Default::default()
                    },
                    TaskArtifact {
                        id: "expired-artifact".to_owned(),
                        kind: TaskArtifactKind::Subtitle.into(),
                        state: TaskArtifactState::Available.into(),
                        resource: Some(expired.resource.clone()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            vec![pending.clone(), unavailable.clone(), expired],
        )
        .expect("persisted output should be valid");
        TaskStateStore::new(&state_path)
            .save(&[record])
            .expect("fixture should persist");
        std::fs::create_dir(state_path.with_file_name("tasks.json.tmp"))
            .expect("directory should block startup rewrite");

        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &state_path,
            TaskRetentionPolicy::default(),
            Some(root_path),
        );

        assert!(!registry.persistence_available());
        assert_eq!(
            3,
            registry
                .task_output_snapshot("owned-resource-task")
                .expect("owned output should restore")
                .output
                .record
                .resources
                .len()
        );
        assert!(pending_path.exists());
        assert!(unavailable_path.exists());
        assert!(expired_path.exists());

        std::fs::remove_dir(state_path.with_file_name("tasks.json.tmp"))
            .expect("startup rewrite blocker should be removable");
        assert!(registry.retry_pending_persistence());
        assert!(registry.task_output_v2_available());
        assert_eq!(
            3,
            registry
                .task_output_snapshot("owned-resource-task")
                .expect("durable output should remain visible")
                .output
                .record
                .resources
                .len()
        );
        assert!(pending_path.exists());
        assert!(unavailable_path.exists());
        assert!(
            expired_path.exists(),
            "startup cleanup must not precede durable expiry retirement"
        );

        let retired = registry
            .retain_task_output_snapshot(
                "owned-resource-task",
                Instant::now() + Duration::from_secs(60),
            )
            .expect("expired resource retirement should become durable");
        assert_eq!(2, retired.output.record.resources.len());
        assert!(!expired_path.exists());
        assert!(pending_path.exists());
        assert!(unavailable_path.exists());

        registry
            .replace_task_output(
                "owned-resource-task",
                vec![TaskResult {
                    id: "owned-resource-result".to_owned(),
                    state: TaskState::Completed.into(),
                    artifacts: vec![
                        TaskArtifact {
                            id: "pending-artifact".to_owned(),
                            kind: TaskArtifactKind::Subtitle.into(),
                            state: TaskArtifactState::Available.into(),
                            resource: Some(pending.resource.clone()),
                            ..Default::default()
                        },
                        TaskArtifact {
                            id: "unavailable-artifact".to_owned(),
                            kind: TaskArtifactKind::Subtitle.into(),
                            state: TaskArtifactState::Available.into(),
                            resource: Some(unavailable.resource.clone()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                vec![pending, unavailable],
            )
            .expect("owned bodies should survive a later availability transition");
        assert!(
            registry
                .open_task_resource("pending-owned-resource")
                .expect("pending resource storage should remain readable")
                .is_some()
        );
        assert!(
            registry
                .open_task_resource("unavailable-owned-resource")
                .expect("unavailable resource storage should remain readable")
                .is_some()
        );
    }

    #[test]
    fn replacement_claims_resource_before_startup_scan_recovery_and_body_write() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("state").join("tasks.json");
        let root_path = temp.path().join("cache");
        let mut expired = test_task_resource("expired-before-replacement", 7);
        expired.resource.expires_at = Some(Timestamp {
            seconds: 0,
            nanos: 0,
        });
        let expired_path = write_task_resource_body(&root_path, &expired, b"expired");

        let mut record = persisted_task_record("staged-resource-task", "BV1staged-resource");
        record.output = TaskOutputRecord::replace(
            Some(&record.output),
            vec![test_task_result_with_resources(
                "expired-result",
                std::slice::from_ref(&expired),
            )],
            vec![expired],
        )
        .expect("persisted output should be valid");
        TaskStateStore::new(&state_path)
            .save(&[record])
            .expect("fixture should persist");
        let rewrite_blocker = state_path.with_file_name("tasks.json.tmp");
        std::fs::create_dir(&rewrite_blocker).expect("directory should block startup rewrite");

        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            &state_path,
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        assert!(!registry.persistence_available());
        assert!(!registry.task_output_v2_available());
        assert!(expired_path.exists());

        std::fs::remove_dir(&rewrite_blocker).expect("startup rewrite blocker should be removable");
        let replacement = test_task_resource("staged-during-startup-scan", 5);
        let staged = registry
            .stage_task_output_replacement("staged-resource-task", vec![replacement])
            .expect("replacement should recover persistence before body creation");
        let replacement = staged
            .resources_requiring_body_creation()
            .next()
            .expect("a new resource should require body creation")
            .clone();
        let replacement_path = write_task_resource_body(&root_path, &replacement, b"fresh");
        staged
            .commit(vec![test_task_result_with_resources(
                "replacement-result",
                std::slice::from_ref(&replacement),
            )])
            .expect("replacement should recover persistence without deleting its staged body");

        assert!(registry.persistence_available());
        assert!(registry.task_output_v2_available());
        assert!(!expired_path.exists());
        assert!(replacement_path.exists());
        assert!(
            registry
                .open_task_resource("staged-during-startup-scan")
                .expect("replacement resource storage should remain readable")
                .is_some()
        );
    }

    #[test]
    fn staging_waits_for_inflight_cleanup_before_resource_body_creation() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("state").join("tasks.json");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = Arc::new(
            BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
                &state_path,
                TaskRetentionPolicy::default(),
                Some(root_path.clone()),
            ),
        );
        let task = registry
            .create_bilibili_task("BV1cleanup-serialized-stage", None)
            .expect("task should be created");
        let resource = test_task_resource("cleanup-serialized-resource", 5);
        let resource_path = write_task_resource_body(&root_path, &resource, b"stale");
        registry
            .inner
            .lock()
            .expect("task registry lock poisoned")
            .durable_resource_cleanup_ids
            .insert(resource.resource.id.clone());

        let (candidate_sender, candidate_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let cleanup_registry = Arc::clone(&registry);
        let cleanup = std::thread::spawn(move || {
            cleanup_registry.cleanup_durable_resource_bodies_with_predelete_hook(|| {
                candidate_sender
                    .send(())
                    .expect("cleanup candidate should be observable");
                release_receiver
                    .recv()
                    .expect("cleanup test should release deletion");
            })
        });
        candidate_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup should select the stale body");

        let (stage_started_sender, stage_started_receiver) = std::sync::mpsc::channel();
        let (stage_acquired_sender, stage_acquired_receiver) = std::sync::mpsc::channel();
        let stage_registry = Arc::clone(&registry);
        let stage_root_path = root_path.clone();
        let stage_task_id = task.id.clone();
        let stage_resource_path = resource_path.clone();
        let stage = std::thread::spawn(move || {
            stage_started_sender
                .send(())
                .expect("staging attempt should be observable");
            let staged = stage_registry
                .stage_task_output_replacement(&stage_task_id, vec![resource])
                .expect("staging should wait for cleanup and then acquire ownership");
            stage_acquired_sender
                .send(())
                .expect("staging claim should be observable");
            assert!(
                !stage_resource_path.exists(),
                "the prior cleanup must finish before the claim permits body creation"
            );
            let staged_resource = staged
                .resources_requiring_body_creation()
                .next()
                .expect("a cleaned resource id should require body creation")
                .clone();
            write_task_resource_body(&stage_root_path, &staged_resource, b"fresh");
            staged
                .commit(vec![test_task_result_with_resources(
                    "cleanup-serialized-result",
                    std::slice::from_ref(&staged_resource),
                )])
                .expect("fresh staged body should commit")
        });
        stage_started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("staging thread should start");
        assert!(
            stage_acquired_receiver
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "staging must not return while a cleanup candidate can still delete the body"
        );

        release_sender
            .send(())
            .expect("cleanup deletion should be released");
        assert!(cleanup.join().expect("cleanup thread should finish"));
        stage_acquired_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("staging should acquire ownership after cleanup finishes");
        let committed = stage.join().expect("staging thread should finish");

        assert_eq!(task.id, committed.id);
        assert_eq!(b"fresh", std::fs::read(&resource_path).unwrap().as_slice());
        assert!(
            registry
                .open_task_resource("cleanup-serialized-resource")
                .expect("committed resource should remain readable")
                .is_some()
        );
    }

    #[test]
    fn generic_task_output_round_trips_with_summary_and_resource_state() {
        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1generic-output", None)
            .expect("task should be created");
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "subtitle-resource".to_owned(),
            content_type: "text/vtt".to_owned(),
            size_bytes: 12,
            size_known: true,
            supports_byte_ranges: true,
            etag: "subtitle-v1".to_owned(),
            ..Default::default()
        })
        .expect("resource should be valid");
        let updated = registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "episode-one".to_owned(),
                    state: TaskState::Completed.into(),
                    title: "Episode one".to_owned(),
                    artifacts: vec![TaskArtifact {
                        id: "subtitle-artifact".to_owned(),
                        kind: TaskArtifactKind::Subtitle.into(),
                        state: TaskArtifactState::Available.into(),
                        resource: Some(resource.resource.clone()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                vec![resource],
            )
            .expect("generic output should be stored");
        let summary = updated
            .output_summary
            .expect("generic output should populate a summary");
        assert_eq!(1, summary.result_count);
        assert_eq!(1, summary.available_artifact_count);
        assert_eq!("episode-one", summary.primary_result_id);

        drop(registry);
        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let snapshot = restored
            .task_output_snapshot(&task.id)
            .expect("generic output should restore");
        assert_eq!(summary.revision, snapshot.revision);
        assert_eq!("episode-one", snapshot.output.record.results[0].id);
        let restored_resource = restored
            .task_resource("subtitle-resource")
            .expect("available resource should restore");
        assert_eq!(
            "/resources/subtitle-resource",
            restored_resource.resource.uri
        );
        assert_eq!(
            ".tvos-net-player/resources/subtitle-resource/body",
            restored_resource.relative_path()
        );
    }

    #[test]
    fn unavailable_snapshot_resource_stays_reserved_without_becoming_servable() {
        use crate::generated::tvos_net_player::v1::{
            TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1unavailable-snapshot", None)
            .expect("task should be created");
        let resource = test_task_resource("unavailable-snapshot-resource", 4);
        let resource_path = write_task_resource_body(&root_path, &resource, b"test");
        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "unavailable-result".to_owned(),
                    state: TaskState::Completed.into(),
                    artifacts: vec![TaskArtifact {
                        id: "unavailable-artifact".to_owned(),
                        kind: TaskArtifactKind::Subtitle.into(),
                        state: TaskArtifactState::Unavailable.into(),
                        resource: Some(resource.resource.clone()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                vec![resource.clone()],
            )
            .expect("unavailable resource metadata should commit");
        let snapshot = registry
            .retain_task_output_snapshot(&task.id, Instant::now() + Duration::from_secs(60))
            .expect("result page snapshot should retain every referenced resource id");

        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "replacement".to_owned(),
                    state: TaskState::Completed.into(),
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect("current output should release the resource");

        assert!(resource_path.exists());
        assert!(
            registry
                .task_resource("unavailable-snapshot-resource")
                .is_none()
        );
        let other_task = registry
            .create_bilibili_task("BV1snapshot-collision", None)
            .expect("second task should be created");
        let collision = match registry
            .stage_task_output_replacement(&other_task.id, vec![resource.clone()])
        {
            Ok(_) => panic!("an unexpired result page must reserve every referenced resource id"),
            Err(error) => error,
        };
        assert_eq!(tonic::Code::AlreadyExists, collision.code());
        assert_eq!(b"test", std::fs::read(&resource_path).unwrap().as_slice());

        registry.release_task_output_snapshots(&[snapshot.resource_lease_id]);
        assert!(!resource_path.exists());
        let staged = registry
            .stage_task_output_replacement(&other_task.id, vec![resource])
            .expect("released snapshot resource id should become reusable");
        assert_eq!(1, staged.resources_requiring_body_creation().count());
    }

    #[test]
    fn retired_resource_body_is_kept_for_snapshot_then_removed_after_expiry() {
        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1resource-retention", None)
            .expect("task should be created");
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "Subtitle-Retained".to_owned(),
            content_type: "text/vtt".to_owned(),
            size_bytes: 4,
            size_known: true,
            etag: "v1".to_owned(),
            ..Default::default()
        })
        .unwrap();
        let resource_path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(resource_path.parent().unwrap()).unwrap();
        std::fs::write(&resource_path, b"test").unwrap();
        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "result-one".to_owned(),
                    state: TaskState::Completed.into(),
                    artifacts: vec![TaskArtifact {
                        id: "subtitle-one".to_owned(),
                        kind: TaskArtifactKind::Subtitle.into(),
                        state: TaskArtifactState::Available.into(),
                        resource: Some(resource.resource.clone()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                vec![resource],
            )
            .unwrap();
        let first_snapshot = registry
            .retain_task_output_snapshot(&task.id, Instant::now() + Duration::from_secs(60))
            .unwrap();
        let second_snapshot = registry
            .retain_task_output_snapshot(&task.id, Instant::now() + Duration::from_secs(60))
            .unwrap();
        assert_eq!(first_snapshot.snapshot_id, second_snapshot.snapshot_id);
        assert!(Arc::ptr_eq(&first_snapshot.output, &second_snapshot.output));
        assert_ne!(
            first_snapshot.resource_lease_id,
            second_snapshot.resource_lease_id
        );

        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "replacement".to_owned(),
                    state: TaskState::Completed.into(),
                    ..Default::default()
                }],
                Vec::new(),
            )
            .unwrap();

        assert!(resource_path.exists());
        assert!(registry.task_resource("SUBTITLE-RETAINED").is_some());
        registry.release_task_output_snapshots(&[first_snapshot.resource_lease_id]);
        assert!(resource_path.exists());
        assert!(registry.task_resource("subtitle-retained").is_some());
        registry.release_task_output_snapshots(&[second_snapshot.resource_lease_id]);
        assert!(!resource_path.exists());
        assert!(!resource_path.parent().unwrap().exists());
        assert!(registry.task_resource("subtitle-retained").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resource_cleanup_reenables_v2_after_a_transient_filesystem_failure() {
        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1resource-cleanup-recovery", None)
            .expect("task should be created");
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "cleanup-recovery".to_owned(),
            content_type: "text/plain".to_owned(),
            size_bytes: 4,
            size_known: true,
            etag: "v1".to_owned(),
            ..Default::default()
        })
        .expect("resource should be valid");
        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "result-one".to_owned(),
                    state: TaskState::Completed.into(),
                    artifacts: vec![TaskArtifact {
                        id: "artifact-one".to_owned(),
                        kind: TaskArtifactKind::Metadata.into(),
                        state: TaskArtifactState::Available.into(),
                        resource: Some(resource.resource.clone()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                vec![resource.clone()],
            )
            .expect("resource output should persist");
        let resource_path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(&resource_path)
            .expect("a directory at the body path should block file cleanup");

        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "replacement".to_owned(),
                    state: TaskState::Completed.into(),
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect("metadata replacement should remain durable");

        assert!(!registry.task_output_v2_available());
        std::fs::remove_dir(&resource_path).expect("cleanup blocker should be removable");
        assert!(registry.cleanup_durable_resource_bodies());
        assert!(registry.task_output_v2_available());
        assert!(!resource_path.parent().unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn missing_resource_root_keeps_retired_id_reserved_until_cleanup_retries() {
        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let unavailable_root_path = temp.path().join("cache-unavailable");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1resource-root-unavailable", None)
            .expect("task should be created");
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "root-unavailable-resource".to_owned(),
            content_type: "text/plain".to_owned(),
            size_bytes: 4,
            size_known: true,
            etag: "v1".to_owned(),
            ..Default::default()
        })
        .expect("resource should be valid");
        let resource_path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(resource_path.parent().unwrap()).unwrap();
        std::fs::write(&resource_path, b"old!").unwrap();
        let artifact = TaskArtifact {
            id: "artifact-one".to_owned(),
            kind: TaskArtifactKind::Metadata.into(),
            state: TaskArtifactState::Available.into(),
            resource: Some(resource.resource.clone()),
            ..Default::default()
        };
        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "result-one".to_owned(),
                    state: TaskState::Completed.into(),
                    artifacts: vec![artifact.clone()],
                    ..Default::default()
                }],
                vec![resource.clone()],
            )
            .expect("resource output should persist");

        std::fs::rename(&root_path, &unavailable_root_path)
            .expect("cache root should become temporarily unavailable");
        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "replacement".to_owned(),
                    state: TaskState::Completed.into(),
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect("metadata retirement should remain durable");

        assert!(!registry.task_output_v2_available());
        let reuse_error = registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "reused".to_owned(),
                    state: TaskState::Completed.into(),
                    artifacts: vec![artifact],
                    ..Default::default()
                }],
                vec![resource],
            )
            .expect_err("an unavailable root must keep the retired id reserved");
        assert_eq!(tonic::Code::FailedPrecondition, reuse_error.code());

        std::fs::rename(&unavailable_root_path, &root_path)
            .expect("cache root should become available again");
        assert!(registry.cleanup_durable_resource_bodies());
        assert!(registry.task_output_v2_available());
        assert!(!resource_path.exists());
        assert!(!resource_path.parent().unwrap().exists());
    }

    #[test]
    fn opened_resource_body_survives_retained_lease_release() {
        use std::io::Read as _;

        use crate::generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState,
        };

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1resource-open-lease", None)
            .expect("task should be created");
        let resource = TaskResourceRecord::new(CacheResourceRef {
            id: "opened-resource".to_owned(),
            content_type: "text/plain".to_owned(),
            size_bytes: 4,
            size_known: true,
            etag: "v1".to_owned(),
            ..Default::default()
        })
        .expect("resource should be valid");
        let resource_path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(resource_path.parent().unwrap()).unwrap();
        std::fs::write(&resource_path, b"test").unwrap();
        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "result-one".to_owned(),
                    state: TaskState::Completed.into(),
                    artifacts: vec![TaskArtifact {
                        id: "artifact-one".to_owned(),
                        kind: TaskArtifactKind::Metadata.into(),
                        state: TaskArtifactState::Available.into(),
                        resource: Some(resource.resource.clone()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                vec![resource],
            )
            .expect("resource output should persist");
        let retained = registry
            .retain_task_output_snapshot(&task.id, Instant::now() + Duration::from_secs(60))
            .expect("output snapshot should retain its resource");
        registry
            .replace_task_output(
                &task.id,
                vec![TaskResult {
                    id: "replacement".to_owned(),
                    state: TaskState::Completed.into(),
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect("resource retirement should persist");

        let mut opened = registry
            .open_task_resource("opened-resource")
            .expect("retained resource storage should remain readable")
            .expect("retained resource should authorize an atomic open");
        registry.release_task_output_snapshots(&[retained.resource_lease_id]);
        assert!(!resource_path.exists());

        let mut body = Vec::new();
        opened
            .file
            .read_to_end(&mut body)
            .expect("the opened descriptor should pin the authorized body object");
        assert_eq!(b"test", body.as_slice());
    }

    #[test]
    fn task_resource_expiry_is_rechecked_after_preopen_blocking_work() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1resource-expiry-lock", None)
            .expect("task should be created");
        let expires_at = SystemTime::now() + Duration::from_secs(2);
        let expires_since_epoch = expires_at
            .duration_since(UNIX_EPOCH)
            .expect("test expiry should follow the Unix epoch");
        let mut resource = test_task_resource("expiry-lock-resource", 4);
        resource.resource.expires_at = Some(Timestamp {
            seconds: expires_since_epoch.as_secs().try_into().unwrap(),
            nanos: expires_since_epoch.subsec_nanos().try_into().unwrap(),
        });
        write_task_resource_body(&root_path, &resource, b"test");
        registry
            .replace_task_output(
                &task.id,
                vec![test_task_result_with_resources(
                    "expiry-lock-result",
                    std::slice::from_ref(&resource),
                )],
                vec![resource],
            )
            .expect("expiring resource should persist");

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let open = scope.spawn(|| {
                registry.open_task_resource_with_prelock_hook("expiry-lock-resource", move || {
                    entered_tx
                        .send(())
                        .expect("test should observe pre-open hook");
                    release_rx
                        .recv_timeout(Duration::from_secs(3))
                        .expect("test should release pre-open hook");
                })
            });
            entered_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("resource open should reach the pre-lock boundary");
            if let Ok(remaining) = expires_at.duration_since(SystemTime::now()) {
                std::thread::sleep(remaining + Duration::from_millis(20));
            }
            release_tx.send(()).expect("resource open should resume");

            assert!(
                open.join()
                    .expect("resource open thread should not panic")
                    .expect("expired authorization should not be a storage failure")
                    .is_none(),
                "expiry must be evaluated after blocking before the protected open"
            );
        });
    }

    #[test]
    fn task_resource_open_failure_degrades_v2_until_storage_revalidates() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        std::fs::create_dir_all(&root_path).expect("cache root should be created");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );
        let task = registry
            .create_bilibili_task("BV1resource-storage-revalidation", None)
            .expect("task should be created");
        let resource = test_task_resource("storage-revalidation-resource", 4);
        write_task_resource_body(&root_path, &resource, b"test");
        registry
            .replace_task_output(
                &task.id,
                vec![test_task_result_with_resources(
                    "storage-revalidation-result",
                    std::slice::from_ref(&resource),
                )],
                vec![resource],
            )
            .expect("resource output should persist");
        assert!(registry.task_output_v2_available());

        let managed_path = root_path.join(".tvos-net-player");
        let unavailable_path = root_path.join(".tvos-net-player-unavailable");
        std::fs::rename(&managed_path, &unavailable_path)
            .expect("test should remove the configured intermediate directory");
        let error = match registry.open_task_resource("storage-revalidation-resource") {
            Err(error) => error,
            Ok(_) => panic!("missing managed storage must be an operational failure"),
        };
        assert_eq!(io::ErrorKind::NotFound, error.kind());
        assert!(!registry.task_output_v2_available());

        assert!(registry.retry_pending_persistence());
        assert!(
            !registry.task_output_v2_available(),
            "a persistence retry must not clear an unresolved storage failure"
        );

        std::fs::rename(&unavailable_path, &managed_path)
            .expect("test should restore the exact managed directory");
        assert!(registry.retry_pending_persistence());
        assert!(registry.task_output_v2_available());
        assert!(
            registry
                .open_task_resource("storage-revalidation-resource")
                .expect("restored storage should pass secure revalidation")
                .is_some()
        );
    }

    #[test]
    fn startup_removes_orphaned_resource_body_before_id_reuse() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let resource_path = root_path.join(".tvos-net-player/resources/orphan-one/body");
        std::fs::create_dir_all(resource_path.parent().unwrap()).unwrap();
        std::fs::write(&resource_path, b"stale").unwrap();

        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path),
        );

        assert!(registry.persistence_available());
        assert!(!resource_path.exists());
        assert!(!resource_path.parent().unwrap().exists());
    }

    #[test]
    fn missing_cache_root_keeps_resource_v2_disabled_until_a_retry_can_scan_it() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("missing-cache");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path.clone()),
        );

        assert!(registry.persistence_available());
        assert!(!registry.task_output_v2_available());

        std::fs::create_dir(&root_path).expect("cache root should become available");
        registry
            .create_bilibili_task("BV1resource-root-recovery", None)
            .expect("a durable mutation should retry the resource scan");

        assert!(registry.task_output_v2_available());
    }

    #[test]
    fn noncanonical_resource_directory_keeps_v2_disabled_until_removed() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let noncanonical_directory = root_path.join(".tvos-net-player/resources/Cover-One");
        std::fs::create_dir_all(&noncanonical_directory)
            .expect("noncanonical resource directory should be created");
        std::fs::write(noncanonical_directory.join("body"), b"stale")
            .expect("stale resource body should be written");
        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path),
        );

        assert!(registry.persistence_available());
        assert!(!registry.task_output_v2_available());
        assert!(noncanonical_directory.join("body").exists());

        std::fs::remove_dir_all(&noncanonical_directory)
            .expect("operator should remove the ambiguous directory");
        registry
            .create_bilibili_task("BV1resource-name-recovery", None)
            .expect("a durable mutation should retry the resource scan");

        assert!(registry.task_output_v2_available());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_resource_namespace_disables_v2_output() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root_path = temp.path().join("cache");
        let internal_path = root_path.join(".tvos-net-player");
        let outside_path = temp.path().join("outside");
        std::fs::create_dir_all(&internal_path).unwrap();
        std::fs::create_dir(&outside_path).unwrap();
        symlink(outside_path, internal_path.join("resources")).unwrap();

        let registry = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            temp.path().join("state").join("tasks.json"),
            TaskRetentionPolicy::default(),
            Some(root_path),
        );

        assert!(registry.persistence_available());
        assert!(!registry.task_output_v2_available());
    }

    #[test]
    fn legacy_managed_output_revision_advances_with_task_state() {
        let registry = BilibiliTaskRegistry::default();
        let task = registry
            .create_bilibili_task("BV1legacy-output", None)
            .expect("task should be created");
        let queued = registry
            .task_output_snapshot(&task.id)
            .expect("queued output should exist");

        let claimed = registry
            .try_claim_next_bilibili_task()
            .expect("task should be claimed");
        assert_eq!(task.id, claimed.task_id);
        let running = registry
            .task_output_snapshot(&task.id)
            .expect("running output should exist");

        assert!(running.revision > queued.revision);
        assert_ne!(running.snapshot_id, queued.snapshot_id);
        assert_eq!(TaskState::Running, running.output.record.results[0].state());
        assert_eq!(
            running.revision,
            registry
                .get_task(&task.id)
                .unwrap()
                .output_summary
                .unwrap()
                .revision
        );
    }

    #[test]
    fn oversized_legacy_reconciliation_never_replaces_durable_snapshot() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let creation = registry
            .create_bilibili_playback_task("BV1oversized-legacy", None, None)
            .expect("playback task should be created");
        let durable_task = registry.get_task(&creation.task.id).unwrap();
        let durable_snapshot = std::fs::read(&path).expect("durable snapshot should be readable");
        let oversized_result = BilibiliTaskResultItem {
            id: "result-one".to_owned(),
            selection_id: "page:1".to_owned(),
            title: "x".repeat(crate::task_output::MAX_TASK_RESULT_ENCODED_BYTES + 1),
            subtitle: String::new(),
            source_kind: "video_page".to_owned(),
            content_id: "cid-1".to_owned(),
            index: 1,
            state: TaskState::Preparing.into(),
            message: String::new(),
            library_item_id: String::new(),
            playback_source: None,
            playback_session: None,
        };

        registry
            .update_playback_results(
                &creation.task.id,
                None,
                "Planning result.".to_owned(),
                0.5,
                vec![oversized_result],
            )
            .expect("legacy mutation remains staged in memory");

        assert_eq!(durable_task, registry.get_task(&creation.task.id).unwrap());
        assert_eq!(
            durable_snapshot,
            std::fs::read(&path).expect("durable snapshot should remain readable")
        );
        assert!(!registry.persistence_available());
        assert!(!registry.task_output_v2_available());
        drop(registry);

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        assert!(restored.get_task(&creation.task.id).is_ok());
        assert!(
            restored
                .task_output_snapshot(&creation.task.id)
                .unwrap()
                .output
                .record
                .results
                .iter()
                .all(|result| {
                    result.title.len() <= crate::task_output::MAX_TASK_RESULT_ENCODED_BYTES
                })
        );
    }

    #[test]
    fn unpersisted_progress_does_not_make_output_revision_regress_after_restart() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1revision-restart", None)
            .expect("task should be created");
        registry
            .try_claim_next_bilibili_task()
            .expect("task should be claimed");
        let durable = registry.task_output_snapshot(&task.id).unwrap();
        for index in 1..=3 {
            assert!(registry.update_task_progress(
                &task.id,
                BilibiliTaskProgress {
                    progress: Some(f64::from(index) / 10.0),
                    downloaded_bytes: Some(i64::from(index)),
                    total_bytes: Some(10),
                    message: Some(format!("Progress {index}")),
                },
            ));
        }
        let before_restart = registry.task_output_snapshot(&task.id).unwrap();
        assert_eq!(durable.revision, before_restart.revision);
        drop(registry);

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let after_restart = restored.task_output_snapshot(&task.id).unwrap();
        assert!(after_restart.revision >= before_restart.revision);
    }

    #[test]
    fn older_persistence_snapshot_cannot_overwrite_newer_snapshot() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let persistence = TaskStatePersistence::new(TaskStateStore::new(path.clone()));

        persistence.save_snapshot(&TaskPersistenceSnapshot {
            generation: 2,
            records: vec![persisted_task_record("bilibili-new", "BV1new")],
            resource_cleanup_ids: Vec::new(),
            pruned_task_ids: Vec::new(),
        });
        persistence.save_snapshot(&TaskPersistenceSnapshot {
            generation: 1,
            records: vec![persisted_task_record("bilibili-old", "BV1old")],
            resource_cleanup_ids: Vec::new(),
            pruned_task_ids: Vec::new(),
        });

        let records = TaskStateStore::new(path)
            .load()
            .expect("task state should load");
        assert_eq!(1, records.len());
        assert_eq!("bilibili-new", records[0].task.id);
        assert_eq!("BV1new", records[0].task.source);
    }

    #[test]
    fn retention_prunes_oldest_terminal_tasks_from_persisted_snapshot() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        TaskStateStore::new(path.clone())
            .save(&[
                persisted_task_record_with_state(
                    "terminal-old",
                    "BV1old",
                    TaskKind::BilibiliDownload,
                    TaskState::Cancelled,
                    10,
                ),
                persisted_task_record_with_state(
                    "terminal-mid",
                    "BV1mid",
                    TaskKind::BilibiliDownload,
                    TaskState::Failed,
                    20,
                ),
                persisted_task_record_with_state(
                    "terminal-new",
                    "BV1new",
                    TaskKind::BilibiliDownload,
                    TaskState::Succeeded,
                    30,
                ),
            ])
            .expect("task state should persist");

        let registry = BilibiliTaskRegistry::with_persistence_path_and_retention(
            &path,
            TaskRetentionPolicy::new(Some(2), None),
        );

        assert!(registry.get_task("terminal-old").is_err());
        assert_eq!(
            TaskState::Failed,
            registry
                .get_task("terminal-mid")
                .expect("mid task should be retained")
                .state()
        );
        assert_eq!(
            TaskState::Succeeded,
            registry
                .get_task("terminal-new")
                .expect("new task should be retained")
                .state()
        );

        let records = TaskStateStore::new(path)
            .load()
            .expect("task state should reload");
        let task_ids = records
            .into_iter()
            .map(|record| record.task.id)
            .collect::<Vec<_>>();
        assert_eq!(vec!["terminal-mid", "terminal-new"], task_ids);
    }

    #[test]
    fn retention_prunes_old_terminal_tasks_but_keeps_active_and_completed_hls_tasks() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        TaskStateStore::new(path.clone())
            .save(&[
                persisted_task_record_with_state(
                    "queued-old",
                    "BV1queued",
                    TaskKind::BilibiliDownload,
                    TaskState::Queued,
                    10,
                ),
                persisted_task_record_with_state(
                    "cancelled-old",
                    "BV1cancelled",
                    TaskKind::BilibiliDownload,
                    TaskState::Cancelled,
                    20,
                ),
                persisted_completed_playback_task_record("playback-completed-old", 30),
            ])
            .expect("task state should persist");

        let registry = BilibiliTaskRegistry::with_persistence_path_and_retention(
            &path,
            TaskRetentionPolicy::new(None, Some(Duration::from_secs(1))),
        );

        assert_eq!(
            TaskState::Queued,
            registry
                .get_task("queued-old")
                .expect("active queued task should be retained")
                .state()
        );
        assert!(registry.get_task("cancelled-old").is_err());
        let completed = registry
            .get_task("playback-completed-old")
            .expect("completed HLS playback task should be retained");
        assert_eq!(TaskState::Completed, completed.state());
        assert_eq!(
            "bilibili.hls.playback-completed-old",
            completed.library_item_id
        );

        let records = TaskStateStore::new(path)
            .load()
            .expect("task state should reload");
        let task_ids = records
            .into_iter()
            .map(|record| record.task.id)
            .collect::<Vec<_>>();
        assert_eq!(vec!["queued-old", "playback-completed-old"], task_ids);
    }

    #[test]
    fn restores_terminal_task_without_active_source_dedupe() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1terminal", None)
            .expect("task should be created");
        let cancelled = registry
            .cancel_task(&task.id)
            .expect("task should be cancelled");
        assert_eq!(TaskState::Cancelled, cancelled.state());

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored.get_task(&task.id).expect("task should restore");
        let requeued = restored
            .create_bilibili_task("BV1terminal", None)
            .expect("terminal source should be requeueable");

        assert_eq!(TaskState::Cancelled, restored_task.state());
        assert_ne!(task.id, requeued.id);
    }

    fn persisted_task_record(id: &str, source: &str) -> PersistedTaskRecord {
        let now = current_timestamp();
        persisted_task_record_with_timestamp(
            id,
            source,
            TaskKind::BilibiliDownload,
            TaskState::Queued,
            now,
        )
    }

    fn persisted_task_record_with_state(
        id: &str,
        source: &str,
        kind: TaskKind,
        state: TaskState,
        seconds: i64,
    ) -> PersistedTaskRecord {
        persisted_task_record_with_timestamp(
            id,
            source,
            kind,
            state,
            Timestamp { seconds, nanos: 0 },
        )
    }

    fn persisted_task_record_with_timestamp(
        id: &str,
        source: &str,
        kind: TaskKind,
        state: TaskState,
        timestamp: Timestamp,
    ) -> PersistedTaskRecord {
        let finished_at = is_terminal(state).then(|| copy_timestamp(&timestamp));
        let task = Task {
            id: id.to_owned(),
            kind: kind.into(),
            state: state.into(),
            source: source.to_owned(),
            title: String::new(),
            progress: 0.0,
            downloaded_bytes: 0,
            total_bytes: 0,
            message: QUEUED_MESSAGE.to_owned(),
            library_item_id: String::new(),
            created_at: Some(copy_timestamp(&timestamp)),
            updated_at: Some(copy_timestamp(&timestamp)),
            finished_at,
            playback_source: None,
            playback_session: None,
            bilibili_selection: None,
            result_items: Vec::new(),
            output_summary: None,
        };
        PersistedTaskRecord {
            output: TaskOutputRecord::from_legacy_task(&task),
            task,
            options: None,
            playback_options: None,
        }
    }

    fn persisted_completed_playback_task_record(id: &str, seconds: i64) -> PersistedTaskRecord {
        let library_item_id = format!("bilibili.hls.{id}");
        let mut record = persisted_task_record_with_state(
            id,
            "BV1completed-old",
            TaskKind::BilibiliProgressivePlayback,
            TaskState::Completed,
            seconds,
        );
        record.task.message = PLAYBACK_COMPLETED_MESSAGE.to_owned();
        record.task.library_item_id = library_item_id.clone();
        record.task.playback_source = Some(PlaybackSource {
            item_id: id.to_owned(),
            variant_id: "h264".to_owned(),
            protocol: crate::generated::tvos_net_player::v1::PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{id}/master.m3u8"),
            expires_at: Some(Timestamp { seconds, nanos: 0 }),
        });
        record.task.playback_session = Some(playback_session(id));
        record
    }

    fn download_options(
        quality_preference: &str,
        download_subtitles: bool,
    ) -> BilibiliDownloadOptions {
        BilibiliDownloadOptions {
            quality_preference: quality_preference.to_owned(),
            encoding_preference: String::new(),
            prefer_tv_api: false,
            download_subtitles,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
        }
    }

    fn playback_options(quality_preference: &str) -> BilibiliPlaybackOptions {
        BilibiliPlaybackOptions {
            quality_preference: quality_preference.to_owned(),
            encoding_preference: "h264".to_owned(),
            prefer_tv_api: false,
            audio_language: String::new(),
            playback_policy: None,
        }
    }

    fn playback_session(task_id: &str) -> BilibiliPlaybackSession {
        BilibiliPlaybackSession {
            id: task_id.to_owned(),
            title: "Planned playback".to_owned(),
            content_id: "cid-1".to_owned(),
            selected_variant_id: "h264".to_owned(),
            selected_variant: Some(
                crate::generated::tvos_net_player::v1::BilibiliPlaybackVariant {
                    id: "h264".to_owned(),
                    label: "1920x1080".to_owned(),
                    source_kind: "dash".to_owned(),
                    container: "mp4".to_owned(),
                    video_codec: "avc1.640028".to_owned(),
                    audio_codec: "mp4a.40.2".to_owned(),
                    width: 1920,
                    height: 1080,
                    bitrate: 1_000_000,
                    size_bytes: 10_000_000,
                },
            ),
            variants: Vec::new(),
            transcoding_plan: None,
            effective_policy: None,
        }
    }

    fn playback_source(task_id: &str) -> PlaybackSource {
        PlaybackSource {
            item_id: task_id.to_owned(),
            variant_id: "h264".to_owned(),
            protocol: crate::generated::tvos_net_player::v1::PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test:8080/hls/{task_id}/master.m3u8"),
            expires_at: None,
        }
    }

    fn test_task_resource(id: &str, size_bytes: i64) -> TaskResourceRecord {
        TaskResourceRecord::new(crate::generated::tvos_net_player::v1::CacheResourceRef {
            id: id.to_owned(),
            content_type: "text/plain".to_owned(),
            size_bytes,
            size_known: true,
            ..Default::default()
        })
        .expect("test resource should be valid")
    }

    fn test_task_result_with_resources(id: &str, resources: &[TaskResourceRecord]) -> TaskResult {
        TaskResult {
            id: id.to_owned(),
            state: TaskState::Completed.into(),
            artifacts: resources
                .iter()
                .map(
                    |resource| crate::generated::tvos_net_player::v1::TaskArtifact {
                        id: format!("artifact-{}", resource.resource.id),
                        kind: crate::generated::tvos_net_player::v1::TaskArtifactKind::Metadata
                            .into(),
                        state: TaskArtifactState::Available.into(),
                        resource: Some(resource.resource.clone()),
                        ..Default::default()
                    },
                )
                .collect(),
            ..Default::default()
        }
    }

    fn write_task_resource_body(
        root_path: &Path,
        resource: &TaskResourceRecord,
        body: &[u8],
    ) -> PathBuf {
        let path = root_path.join(resource.relative_path());
        std::fs::create_dir_all(path.parent().unwrap())
            .expect("staged resource directory should be created");
        std::fs::write(&path, body).expect("staged resource body should be written");
        path
    }
}
