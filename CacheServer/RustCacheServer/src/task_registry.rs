use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use prost_types::Timestamp;
use tokio::sync::mpsc;
use tonic::Status;
use uuid::Uuid;

use crate::generated::tvos_net_player::v1::{BilibiliDownloadOptions, Task, TaskKind, TaskState};

const QUEUED_MESSAGE: &str = "Queued for the BBDown adapter.";
const CANCELLED_MESSAGE: &str = "Cancelled before the download adapter started.";
const WATCHER_EVENT_BUFFER_CAPACITY: usize = 128;

pub struct BilibiliTaskRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl BilibiliTaskRegistry {
    pub fn create_bilibili_task(
        &self,
        source: &str,
        _options: Option<BilibiliDownloadOptions>,
    ) -> Result<Task, Status> {
        let normalized_source = normalize(source);
        if normalized_source.is_empty() {
            return Err(Status::invalid_argument("Bilibili URL or id is required."));
        }

        let mut inner = self.inner.lock().expect("task registry lock poisoned");
        if let Some(active_task_id) = inner.active_task_ids_by_source.get(&normalized_source)
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
            created_at: Some(now),
            updated_at: Some(now),
            finished_at: None,
        };

        inner
            .active_task_ids_by_source
            .insert(normalized_source, task.id.clone());
        inner.tasks_by_id.insert(task.id.clone(), task.clone());
        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        Ok(task)
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
        let (task, source, task_id) = {
            let Some(task) = inner.tasks_by_id.get_mut(&normalized_id) else {
                return Err(task_not_found());
            };

            if is_terminal(task.state()) {
                return Ok(task.clone());
            }

            task.state = TaskState::Cancelled.into();
            task.message = CANCELLED_MESSAGE.to_owned();
            task.updated_at = Some(current_timestamp());
            task.finished_at = task.updated_at;

            (task.clone(), task.source.clone(), task.id.clone())
        };

        if inner
            .active_task_ids_by_source
            .get(&source)
            .is_some_and(|active_task_id| active_task_id == &task_id)
        {
            inner.active_task_ids_by_source.remove(&source);
        }

        Self::publish_locked(&mut inner, task.clone());
        drop(inner);
        Ok(task)
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
        Self {
            inner: Arc::new(Mutex::new(RegistryInner::default())),
        }
    }
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
    active_task_ids_by_source: HashMap<String, String>,
    watchers: HashMap<Uuid, TaskWatcher>,
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

fn is_active(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Queued | TaskState::Running | TaskState::CancelRequested
    )
}

fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_active_tasks_by_source() {
        let registry = BilibiliTaskRegistry::default();
        let first = registry
            .create_bilibili_task("  BV1xx  ", None)
            .expect("task should be created");
        let duplicate = registry
            .create_bilibili_task("BV1xx", None)
            .expect("duplicate task should be returned");

        assert_eq!(first.id, duplicate.id);
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
}
