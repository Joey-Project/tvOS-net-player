use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::Duration,
};

use prost_types::Timestamp;
use tokio::sync::{Notify, mpsc};
use tonic::Status;
use uuid::Uuid;

use crate::{
    generated::tvos_net_player::v1::{
        BilibiliDownloadOptions, BilibiliPlaybackOptions, BilibiliPlaybackSession, PlaybackSource,
        Task, TaskKind, TaskState,
    },
    task_store::{PersistedTaskRecord, TaskStateStore},
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
const WATCHER_EVENT_BUFFER_CAPACITY: usize = 128;
const DEFAULT_MAX_TERMINAL_TASKS: usize = 200;
const DEFAULT_TERMINAL_TASK_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub struct BilibiliTaskRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    queue_notify: Arc<Notify>,
    persistence: Option<TaskStatePersistence>,
    retention_policy: TaskRetentionPolicy,
}

impl BilibiliTaskRegistry {
    pub fn with_persistence_path(path: impl Into<PathBuf>) -> Self {
        Self::with_persistence_path_and_retention(path, TaskRetentionPolicy::default())
    }

    pub fn with_persistence_path_and_retention(
        path: impl Into<PathBuf>,
        retention_policy: TaskRetentionPolicy,
    ) -> Self {
        let store = TaskStateStore::new(path);
        let records = match store.load() {
            Ok(records) => records,
            Err(error) => {
                eprintln!(
                    "Failed to load persisted Bilibili task state from {}; task state writeback is disabled until the snapshot is repaired: {error}",
                    store.path().display()
                );
                return Self::from_persisted_records(Vec::new(), None, retention_policy);
            }
        };
        let should_rewrite_snapshot = !records.is_empty();
        let registry = Self::from_persisted_records(records, Some(store), retention_policy);
        if should_rewrite_snapshot {
            registry.persist_current_state();
        }
        registry
    }

    pub fn persistence_available(&self) -> bool {
        self.persistence.is_some()
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

        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let active_key = ActiveBilibiliTaskKey::download(&normalized_source, options.as_ref());
        if let Some(active_task_id) = inner.active_task_ids_by_key.get(&active_key)
            && let Some(active_task) = inner.tasks_by_id.get(active_task_id)
            && is_active(active_task.state())
        {
            return Ok(active_task.clone());
        }

        let now = current_timestamp();
        let task = Task {
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
        };

        inner
            .active_task_ids_by_key
            .insert(active_key, task.id.clone());
        inner
            .download_options_by_id
            .insert(task.id.clone(), options.clone());
        inner.queued_task_ids.push_back(task.id.clone());
        inner.tasks_by_id.insert(task.id.clone(), task.clone());
        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
        self.queue_notify.notify_one();
        Ok(task)
    }

    pub fn create_bilibili_playback_task(
        &self,
        source: &str,
        options: Option<BilibiliPlaybackOptions>,
    ) -> Result<BilibiliPlaybackTaskCreation, Status> {
        let normalized_source = normalize(source);
        if normalized_source.is_empty() {
            return Err(Status::invalid_argument("Bilibili URL or id is required."));
        }

        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let now = current_timestamp();
        let task = Task {
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
        };

        inner
            .playback_options_by_id
            .insert(task.id.clone(), options.clone());
        let cancellation = BilibiliTaskCancellation::default();
        inner
            .planning_cancellations_by_id
            .insert(task.id.clone(), cancellation.clone());
        inner.tasks_by_id.insert(task.id.clone(), task.clone());
        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
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
            .tasks_by_id
            .get(&normalized_id)
            .cloned()
            .ok_or_else(task_not_found)
    }

    pub fn cancel_task(&self, id: &str) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let Some(current_state) = inner
            .tasks_by_id
            .get(&normalized_id)
            .map(|task| task.state())
        else {
            return Err(task_not_found());
        };
        if is_terminal(current_state) {
            return Ok(inner
                .tasks_by_id
                .get(&normalized_id)
                .expect("task must exist after state lookup")
                .clone());
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
                let snapshot = self.persistence_snapshot_locked(&mut inner);
                Self::publish_locked(&mut inner, task.clone());
                drop(inner);
                self.persist_snapshot(snapshot);
                return Ok(task);
            }

            return Ok(inner
                .tasks_by_id
                .get(&normalized_id)
                .expect("task must exist after state lookup")
                .clone());
        }

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
            }

            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);

        Self::clear_active_task_locked(&mut inner, &terminal_task);

        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
        Ok(task)
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
            let snapshot = self.persistence_snapshot_locked(&mut inner);
            Self::publish_locked(&mut inner, task);
            drop(inner);
            self.persist_snapshot(snapshot);
            return Some(work_item);
        }

        None
    }

    pub fn update_task_progress(&self, id: &str, progress: BilibiliTaskProgress) -> bool {
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
        Self::publish_locked(&mut inner, task);
        true
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

        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
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

        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
        Ok(task)
    }

    pub fn complete_playback_cached(
        &self,
        id: &str,
        library_item_id: String,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
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
            if task.playback_source.is_none() || task.playback_session.is_none() {
                return Err(Status::failed_precondition(
                    "Task does not have a playable Bilibili playback session.",
                ));
            }

            if task.state() == TaskState::CancelRequested {
                let finished_at = current_timestamp();
                task.state = TaskState::Cancelled.into();
                task.message = CANCELLED_BY_REQUEST_MESSAGE.to_owned();
                task.library_item_id.clear();
                task.playback_source = None;
                task.playback_session = None;
                task.updated_at = Some(copy_timestamp(&finished_at));
                task.finished_at = Some(finished_at);
            } else {
                let finished_at = current_timestamp();
                task.state = TaskState::Completed.into();
                task.message = PLAYBACK_COMPLETED_MESSAGE.to_owned();
                task.library_item_id = library_item_id.clone();
                if let Some(playback_source) = task.playback_source.as_mut() {
                    playback_source.item_id = library_item_id;
                    playback_source.expires_at = None;
                }
                task.progress = 1.0;
                task.updated_at = Some(copy_timestamp(&finished_at));
                task.finished_at = Some(finished_at);
            }

            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);
        Self::clear_active_task_locked(&mut inner, &terminal_task);
        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
        Ok(task)
    }

    pub fn refresh_playback_source(
        &self,
        id: &str,
        playback_source: PlaybackSource,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
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
            if !matches!(task.state(), TaskState::Playable | TaskState::Completed) {
                return Ok(task.clone());
            }

            task.playback_source = Some(playback_source);
            task.clone()
        };
        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
        Ok(task)
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
            task.message = message;
            task.library_item_id.clear();
            task.playback_source = None;
            task.playback_session = None;
            task.updated_at = Some(copy_timestamp(&finished_at));
            task.finished_at = Some(finished_at);
            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);
        Self::clear_active_task_locked(&mut inner, &terminal_task);
        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
        Ok(task)
    }

    pub fn fail_completed_playback_task_after_cache_restore(
        &self,
        id: &str,
        message: String,
    ) -> Result<Task, Status> {
        let normalized_id = normalize_required_id(id)?;
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
            if task.state() != TaskState::Completed {
                return Ok(task.clone());
            }

            let finished_at = current_timestamp();
            task.state = TaskState::Failed.into();
            task.message = message;
            task.library_item_id.clear();
            task.playback_source = None;
            task.playback_session = None;
            task.updated_at = Some(copy_timestamp(&finished_at));
            task.finished_at = Some(finished_at);
            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);
        Self::clear_active_task_locked(&mut inner, &terminal_task);
        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
        Ok(task)
    }

    pub fn remove_completed_playback_task(
        &self,
        id: &str,
        library_item_id: &str,
    ) -> Result<bool, Status> {
        let normalized_id = normalize_required_id(id)?;
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let Some(task) = inner.tasks_by_id.get(&normalized_id) else {
            return Ok(false);
        };
        if task.kind() != TaskKind::BilibiliProgressivePlayback {
            return Err(Status::failed_precondition(
                "Task is not a Bilibili progressive playback task.",
            ));
        }
        if task.state() != TaskState::Completed || task.library_item_id != library_item_id {
            return Err(Status::failed_precondition(
                "Only completed playback tasks matching the deleted cache item can be removed.",
            ));
        }

        let mut removed_task = inner
            .tasks_by_id
            .remove(&normalized_id)
            .expect("task must exist after precondition checks");
        let finished_at = current_timestamp();
        removed_task.state = TaskState::Failed.into();
        removed_task.message = PLAYBACK_CACHE_DELETED_MESSAGE.to_owned();
        removed_task.library_item_id.clear();
        removed_task.playback_source = None;
        removed_task.playback_session = None;
        removed_task.updated_at = Some(copy_timestamp(&finished_at));
        removed_task.finished_at = Some(finished_at);
        inner.download_options_by_id.remove(&normalized_id);
        inner.playback_options_by_id.remove(&normalized_id);
        inner.running_cancellations_by_id.remove(&normalized_id);
        inner.planning_cancellations_by_id.remove(&normalized_id);
        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, removed_task);
        drop(inner);
        self.persist_snapshot(snapshot);
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
        inner.tasks_by_id.get(&normalized_id).is_some_and(|task| {
            task.kind() == TaskKind::BilibiliProgressivePlayback
                && task.state() == TaskState::Playable
        })
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
            .tasks_by_id
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
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let mut changed_tasks = Vec::new();
        let mut changed_task_ids = Vec::new();
        for task in inner.tasks_by_id.values_mut() {
            if task.kind() != TaskKind::BilibiliProgressivePlayback {
                continue;
            }
            let is_restorable = match task.state() {
                TaskState::Playable => restorable_playable_session_ids.contains(&task.id),
                TaskState::Completed => restorable_completed_session_ids.contains(&task.id),
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
            task.updated_at = Some(copy_timestamp(&updated_at));
            task.finished_at = Some(updated_at);
            changed_task_ids.push(task.id.clone());
            changed_tasks.push(task.clone());
        }
        if changed_tasks.is_empty() {
            return changed_task_ids;
        }

        let snapshot = self.persistence_snapshot_locked(&mut inner);
        for task in changed_tasks {
            Self::publish_locked(&mut inner, task);
        }
        drop(inner);
        self.persist_snapshot(snapshot);
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
        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        let task = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };
            if is_terminal(task.state()) {
                return Ok(task.clone());
            }

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
            task.message = effective_message;
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
            let finished_at = current_timestamp();
            task.updated_at = Some(copy_timestamp(&finished_at));
            task.finished_at = Some(finished_at);

            task.clone()
        };
        let terminal_task = Self::terminal_task_locked(&inner, &task);

        Self::clear_active_task_locked(&mut inner, &terminal_task);
        let snapshot = self.persistence_snapshot_locked(&mut inner);
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        self.persist_snapshot(snapshot);
        Ok(task)
    }

    fn from_persisted_records(
        records: Vec<PersistedTaskRecord>,
        store: Option<TaskStateStore>,
        retention_policy: TaskRetentionPolicy,
    ) -> Self {
        let mut inner = RegistryInner::default();
        for record in records {
            let Some((task, download_options, playback_options)) = restore_persisted_record(record)
            else {
                continue;
            };

            let is_active_task = is_active(task.state());
            let task_id = task.id.clone();
            if is_active_task && task.kind() == TaskKind::BilibiliDownload {
                let active_key = active_key_for_task(
                    &task,
                    download_options.as_ref(),
                    playback_options.as_ref(),
                );
                if inner.active_task_ids_by_key.contains_key(&active_key) {
                    continue;
                }
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
            inner.tasks_by_id.insert(task_id, task);
        }

        Self {
            inner: Arc::new(Mutex::new(inner)),
            queue_notify: Arc::new(Notify::new()),
            persistence: store.map(TaskStatePersistence::new),
            retention_policy,
        }
    }

    fn persist_current_state(&self) {
        let snapshot = {
            let mut inner = self.inner.lock().expect("task registry lock poisoned");
            self.persistence_snapshot_locked(&mut inner)
        };
        self.persist_snapshot(snapshot);
    }

    fn persistence_snapshot_locked(
        &self,
        inner: &mut RegistryInner,
    ) -> Option<TaskPersistenceSnapshot> {
        self.persistence.as_ref()?;
        prune_terminal_tasks_locked(inner, &self.retention_policy, &current_timestamp());
        inner.persistence_generation += 1;
        Some(TaskPersistenceSnapshot {
            generation: inner.persistence_generation,
            records: persisted_records_locked(inner),
        })
    }

    fn persist_snapshot(&self, snapshot: Option<TaskPersistenceSnapshot>) {
        if let Some(snapshot) = snapshot
            && let Some(persistence) = &self.persistence
        {
            persistence.save_snapshot(snapshot);
        }
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
}

impl Default for BilibiliTaskRegistry {
    fn default() -> Self {
        Self::from_persisted_records(Vec::new(), None, TaskRetentionPolicy::default())
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
    download_options_by_id: HashMap<String, Option<BilibiliDownloadOptions>>,
    playback_options_by_id: HashMap<String, Option<BilibiliPlaybackOptions>>,
    active_task_ids_by_key: HashMap<ActiveBilibiliTaskKey, String>,
    queued_task_ids: VecDeque<String>,
    running_cancellations_by_id: HashMap<String, BilibiliTaskCancellation>,
    planning_cancellations_by_id: HashMap<String, BilibiliTaskCancellation>,
    watchers: HashMap<Uuid, TaskWatcher>,
    persistence_generation: u64,
}

struct TaskStatePersistence {
    store: TaskStateStore,
    state: Mutex<TaskStatePersistenceState>,
}

impl TaskStatePersistence {
    fn new(store: TaskStateStore) -> Self {
        Self {
            store,
            state: Mutex::new(TaskStatePersistenceState::default()),
        }
    }

    fn path(&self) -> &Path {
        self.store.path()
    }

    fn save_snapshot(&self, snapshot: TaskPersistenceSnapshot) {
        let mut state = self.state.lock().expect("task persistence lock poisoned");
        if snapshot.generation < state.latest_seen_generation {
            return;
        }
        state.latest_seen_generation = snapshot.generation;

        if let Err(error) = self.store.save(&snapshot.records) {
            eprintln!(
                "Failed to persist Bilibili task state to {}: {error}",
                self.path().display()
            );
        }
    }
}

#[derive(Default)]
struct TaskStatePersistenceState {
    latest_seen_generation: u64,
}

struct TaskPersistenceSnapshot {
    generation: u64,
    records: Vec<PersistedTaskRecord>,
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
    prefer_tv_api: bool,
    download_subtitles: bool,
    download_danmaku: bool,
}

impl ActiveBilibiliTaskKey {
    fn download(source: &str, options: Option<&BilibiliDownloadOptions>) -> Self {
        let mut key = Self::new(TaskKind::BilibiliDownload, source);
        if let Some(options) = options {
            key.quality_preference = normalize_option_string(&options.quality_preference);
            key.encoding_preference = normalize_option_string(&options.encoding_preference);
            key.prefer_tv_api = options.prefer_tv_api;
            key.download_subtitles = options.download_subtitles;
            key.download_danmaku = options.download_danmaku;
        }
        key
    }

    fn playback(source: &str, options: Option<&BilibiliPlaybackOptions>) -> Self {
        let mut key = Self::new(TaskKind::BilibiliProgressivePlayback, source);
        if let Some(options) = options {
            key.quality_preference = normalize_option_string(&options.quality_preference);
            key.encoding_preference = normalize_option_string(&options.encoding_preference);
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
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
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
        task.updated_at = Some(copy_timestamp(&updated_at));
        task.finished_at = Some(updated_at);
    }

    Some((task, record.options, record.playback_options))
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

fn prune_terminal_tasks_locked(
    inner: &mut RegistryInner,
    policy: &TaskRetentionPolicy,
    now: &Timestamp,
) {
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

    for task_id in prune_ids {
        inner.tasks_by_id.remove(&task_id);
        inner.download_options_by_id.remove(&task_id);
        inner.playback_options_by_id.remove(&task_id);
        inner.running_cancellations_by_id.remove(&task_id);
        inner.planning_cancellations_by_id.remove(&task_id);
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
            .create_bilibili_playback_task("BV1play", Some(playback_options("720P")))
            .expect("playback task should be created");
        let repeated_playback = registry
            .create_bilibili_playback_task("  BV1play  ", Some(playback_options("720p")))
            .expect("repeated playback task should be created");
        let different_playback = registry
            .create_bilibili_playback_task("BV1play", Some(playback_options("1080p")))
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
            .create_bilibili_playback_task("BV1cancel-planning", None)
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
            .create_bilibili_playback_task("BV1cancel-planning", None)
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
            .create_bilibili_playback_task("BV1planned", Some(options.clone()))
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
            .create_bilibili_playback_task("BV1planned", Some(options))
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
            .create_bilibili_playback_task("BV1playable", Some(playback_options("1080p")))
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
            .create_bilibili_playback_task("BV1playable", Some(playback_options("1080p")))
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
            .create_bilibili_playback_task("BV1completed", Some(playback_options("1080p")))
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
            .create_bilibili_playback_task("BV1delete", Some(playback_options("1080p")))
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
            .create_bilibili_playback_task("BV1delete", Some(playback_options("1080p")))
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
        assert!(registry.get_task(&created.task.id).is_err());
    }

    #[test]
    fn remove_completed_playback_task_rejects_mismatched_or_active_records() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1active", Some(playback_options("1080p")))
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
            .create_bilibili_playback_task("BV1refresh", Some(playback_options("1080p")))
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
    fn fails_unrestorable_playable_progressive_playback_tasks() {
        let registry = BilibiliTaskRegistry::default();
        let created = registry
            .create_bilibili_playback_task("BV1missing-manifest", Some(playback_options("1080p")))
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
            .create_bilibili_playback_task("BV1playable", Some(options.clone()))
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
            .create_bilibili_playback_task("BV1playable", Some(options))
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
            .create_bilibili_playback_task("BV1cancel-playable", Some(options.clone()))
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
            .create_bilibili_playback_task("BV1cancel-playable", Some(options))
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
    fn restores_preparing_progressive_playback_task_as_failed() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let created = BilibiliTaskRegistry::with_persistence_path(&path)
            .create_bilibili_playback_task("BV1preparing", None)
            .expect("playback task should be created");

        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        let restored_task = restored
            .get_task(&created.task.id)
            .expect("task should restore");
        let requeued = restored
            .create_bilibili_playback_task("BV1preparing", None)
            .expect("failed playback source should be requeueable");

        assert_eq!(TaskState::Failed, restored_task.state());
        assert_eq!(
            PREPARING_INTERRUPTED_AFTER_RESTART_MESSAGE,
            restored_task.message
        );
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
    fn invalid_persisted_state_disables_writeback_without_overwriting_file() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        std::fs::write(&path, b"{ invalid json").expect("invalid state should be written");

        let registry = BilibiliTaskRegistry::with_persistence_path(&path);
        let task = registry
            .create_bilibili_task("BV1invalid-state", None)
            .expect("registry should remain usable in memory");
        let persisted = std::fs::read_to_string(&path).expect("state file should remain readable");

        assert_eq!(TaskState::Queued, task.state());
        assert_eq!("{ invalid json", persisted);
    }

    #[test]
    fn older_persistence_snapshot_cannot_overwrite_newer_snapshot() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        let persistence = TaskStatePersistence::new(TaskStateStore::new(path.clone()));

        persistence.save_snapshot(TaskPersistenceSnapshot {
            generation: 2,
            records: vec![persisted_task_record("bilibili-new", "BV1new")],
        });
        persistence.save_snapshot(TaskPersistenceSnapshot {
            generation: 1,
            records: vec![persisted_task_record("bilibili-old", "BV1old")],
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
        PersistedTaskRecord {
            task: Task {
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
            },
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
        }
    }

    fn playback_options(quality_preference: &str) -> BilibiliPlaybackOptions {
        BilibiliPlaybackOptions {
            quality_preference: quality_preference.to_owned(),
            encoding_preference: "h264".to_owned(),
            prefer_tv_api: false,
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
}
