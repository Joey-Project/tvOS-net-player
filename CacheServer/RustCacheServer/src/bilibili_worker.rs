use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::Arc,
};

use futures_util::FutureExt;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{
    bilibili_resolution::BilibiliTaskCandidateRecord,
    generated::tvos_net_player::v1::{
        BilibiliDownloadOptions, BilibiliRequestContext, Task, TaskResult, TaskState,
    },
    task_output::TaskResourceRecord,
    task_registry::{
        BilibiliTaskCancellation, BilibiliTaskProgress, BilibiliTaskRegistry, BilibiliTaskWorkItem,
        StagedTaskOutputReplacement, TaskPersistenceRecoveryOutcome,
    },
};

pub type BilibiliDownloadFuture<'a> = Pin<
    Box<dyn Future<Output = Result<BilibiliDownloadOutput, BilibiliDownloadError>> + Send + 'a>,
>;

pub trait BilibiliDownloadAdapter: Send + Sync + 'static {
    fn run<'a>(
        &'a self,
        request: BilibiliDownloadRequest,
        context: BilibiliDownloadContext,
    ) -> BilibiliDownloadFuture<'a>;
}

#[derive(Clone)]
pub struct BilibiliDownloadRequest {
    pub task_id: String,
    pub source: String,
    pub options: Option<BilibiliDownloadOptions>,
    pub request_context: Option<BilibiliRequestContext>,
    pub(crate) candidates: Vec<BilibiliTaskCandidateRecord>,
}

pub struct BilibiliDownloadOutput {
    pub library_item_id: String,
    pub message: String,
    pub v2: Option<BilibiliDownloadOutputV2>,
}

pub struct BilibiliDownloadOutputV2 {
    pub terminal_state: TaskState,
    pub results: Vec<TaskResult>,
    pub(crate) resources: Vec<TaskResourceRecord>,
    pub resource_bodies: Vec<BilibiliTaskResourceBody>,
}

pub struct BilibiliTaskResourceBody {
    pub resource_id: String,
    pub source: BilibiliTaskResourceBodySource,
}

pub enum BilibiliTaskResourceBodySource {
    CachePath(std::path::PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BilibiliDownloadError {
    Failed(String),
    ResourceExhausted(String),
    Cancelled(String),
}

impl BilibiliDownloadError {
    fn message(self) -> String {
        match self {
            Self::Failed(message) | Self::ResourceExhausted(message) | Self::Cancelled(message) => {
                message
            }
        }
    }
}

#[derive(Clone)]
pub struct BilibiliDownloadContext {
    registry: Arc<BilibiliTaskRegistry>,
    task_id: String,
    cancellation: BilibiliTaskCancellation,
}

impl BilibiliDownloadContext {
    pub fn is_cancel_requested(&self) -> bool {
        self.cancellation.is_cancel_requested()
    }

    pub fn report_progress(&self, progress: BilibiliTaskProgress) -> bool {
        self.registry.update_task_progress(&self.task_id, progress)
    }
}

pub async fn run_bilibili_task_worker(
    registry: Arc<BilibiliTaskRegistry>,
    adapter: Arc<dyn BilibiliDownloadAdapter>,
    max_concurrent_tasks: usize,
    credentials_configured: bool,
) {
    let max_concurrent_tasks = max_concurrent_tasks.max(1);
    let semaphore = Arc::new(Semaphore::new(max_concurrent_tasks));
    let mut running_tasks = JoinSet::new();

    loop {
        while let Some(result) = running_tasks.try_join_next() {
            if let Err(error) = result {
                eprintln!("Bilibili task worker task failed: {error}");
            }
        }

        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("worker semaphore must stay open");
        let work_item = claim_next_bilibili_task(Arc::clone(&registry)).await;
        let registry = Arc::clone(&registry);
        let adapter = Arc::clone(&adapter);
        running_tasks.spawn(async move {
            let _permit = permit;
            run_one_bilibili_task(registry, adapter, work_item, credentials_configured).await;
        });
    }
}

async fn claim_next_bilibili_task(registry: Arc<BilibiliTaskRegistry>) -> BilibiliTaskWorkItem {
    loop {
        if let Some(work_item) = registry.try_claim_next_bilibili_task() {
            return work_item;
        }
        if registry.has_bilibili_v2_task_waiting_for_persistence() {
            match retry_pending_task_persistence(&registry).await {
                TaskPersistenceRecoveryOutcome::Durable => continue,
                TaskPersistenceRecoveryOutcome::RetryableFailure => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                TaskPersistenceRecoveryOutcome::PermanentFailure => {
                    eprintln!(
                        "Bilibili v2 task creation persistence recovery was rejected permanently; the task will remain queued"
                    );
                }
            }
        }
        registry.wait_for_bilibili_task_queue_change().await;
    }
}

async fn run_one_bilibili_task(
    registry: Arc<BilibiliTaskRegistry>,
    adapter: Arc<dyn BilibiliDownloadAdapter>,
    work_item: BilibiliTaskWorkItem,
    credentials_configured: bool,
) {
    let request = BilibiliDownloadRequest {
        task_id: work_item.task_id.clone(),
        source: work_item.source,
        options: work_item.options,
        request_context: work_item.request_context,
        candidates: work_item.accepted_candidates,
    };
    let context = BilibiliDownloadContext {
        registry: Arc::clone(&registry),
        task_id: request.task_id.clone(),
        cancellation: work_item.cancellation.clone(),
    };
    let result = match AssertUnwindSafe(async move { adapter.run(request, context).await })
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(_) => {
            let task_id = work_item.task_id.clone();
            complete_terminal_task(&registry, move |registry| {
                registry.complete_task_failed(
                    &task_id,
                    "Bilibili download adapter panicked.".to_owned(),
                )
            })
            .await;
            return;
        }
    };

    if work_item.cancellation.is_cancel_requested()
        && !matches!(&result, Ok(output) if output.v2.is_some())
    {
        let message = match result {
            Err(BilibiliDownloadError::Cancelled(message)) => {
                crate::credential_safe_client_cancellation(credentials_configured, &message)
            }
            _ => "Cancelled by request.".to_owned(),
        };
        let task_id = work_item.task_id.clone();
        complete_terminal_task(&registry, move |registry| {
            registry.complete_task_cancelled(&task_id, message.clone())
        })
        .await;
        return;
    }

    match result {
        Ok(mut output) => {
            if let Some(mut output_v2) = output.v2.take() {
                redact_download_output_v2(&mut output_v2, credentials_configured);
                complete_v2_terminal_task(
                    &registry,
                    work_item.task_id.clone(),
                    output.library_item_id,
                    output.message,
                    output_v2,
                )
                .await;
                return;
            }
            let task_id = work_item.task_id.clone();
            complete_terminal_task(&registry, move |registry| {
                registry.complete_task_succeeded(
                    &task_id,
                    output.library_item_id.clone(),
                    output.message.clone(),
                )
            })
            .await;
        }
        Err(BilibiliDownloadError::Cancelled(message)) => {
            let message =
                crate::credential_safe_client_cancellation(credentials_configured, &message);
            let task_id = work_item.task_id.clone();
            complete_terminal_task(&registry, move |registry| {
                registry.complete_task_cancelled(&task_id, message.clone())
            })
            .await;
        }
        Err(
            error
            @ (BilibiliDownloadError::Failed(_) | BilibiliDownloadError::ResourceExhausted(_)),
        ) => {
            let detail = error.message();
            let message = crate::credential_safe_client_error(credentials_configured, &detail);
            let task_id = work_item.task_id.clone();
            complete_terminal_task(&registry, move |registry| {
                registry.complete_task_failed(&task_id, message.clone())
            })
            .await;
        }
    }
}

fn redact_download_output_v2(output: &mut BilibiliDownloadOutputV2, credentials_configured: bool) {
    if !credentials_configured {
        return;
    }
    for result in &mut output.results {
        match result.state() {
            TaskState::Failed => {
                if let Some(problem) = result.problem.as_mut() {
                    problem.message = crate::credential_safe_client_error(true, &problem.message);
                }
                if let Some(progress) = result.progress.as_mut() {
                    progress.message = crate::credential_safe_client_error(true, &progress.message);
                }
            }
            TaskState::Cancelled => {
                if let Some(problem) = result.problem.as_mut() {
                    problem.message =
                        crate::credential_safe_client_cancellation(true, &problem.message);
                }
                if let Some(progress) = result.progress.as_mut() {
                    progress.message =
                        crate::credential_safe_client_cancellation(true, &progress.message);
                }
            }
            _ => {}
        }
        for artifact in &mut result.artifacts {
            if let Some(problem) = artifact.problem.as_mut() {
                problem.message = crate::credential_safe_client_error(true, &problem.message);
            }
        }
    }
}

async fn complete_v2_terminal_task(
    registry: &Arc<BilibiliTaskRegistry>,
    task_id: String,
    library_item_id: String,
    message: String,
    output: BilibiliDownloadOutputV2,
) {
    let output = Arc::new(output);
    let publication = complete_terminal_task_with_outcome(registry, {
        let task_id = task_id.clone();
        let library_item_id = library_item_id.clone();
        let message = message.clone();
        let output = Arc::clone(&output);
        move |registry| {
            validate_resource_body_descriptors(&output.resources, &output.resource_bodies)
                .map_err(|error| tonic::Status::internal(error.to_string()))?;
            let staged =
                registry.stage_task_output_replacement(&task_id, output.resources.clone())?;
            create_staged_resource_bodies(&staged, &output.resource_bodies)
                .map_err(|error| tonic::Status::internal(error.to_string()))?;
            staged.commit_download_terminal(
                output.results.clone(),
                output.terminal_state,
                library_item_id.clone(),
                message.clone(),
            )
        }
    })
    .await;

    if let Err(error) = publication {
        eprintln!("Failed to publish Bilibili v2 task output for {task_id}: {error}");
        complete_terminal_task(registry, move |registry| {
            registry.complete_task_failed(
                &task_id,
                "Bilibili download output could not be published.".to_owned(),
            )
        })
        .await;
    }
}

fn validate_resource_body_descriptors(
    resources: &[TaskResourceRecord],
    bodies: &[BilibiliTaskResourceBody],
) -> io::Result<()> {
    let resource_ids = resources
        .iter()
        .map(|resource| resource.resource.id.as_str())
        .collect::<HashSet<_>>();
    if resource_ids.len() != resources.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Bilibili task output contains duplicate resource ids",
        ));
    }
    let mut body_ids = HashSet::with_capacity(bodies.len());
    for body in bodies {
        if !body_ids.insert(body.resource_id.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Bilibili task output contains duplicate resource bodies",
            ));
        }
        if !resource_ids.contains(body.resource_id.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Bilibili task output body has no matching resource",
            ));
        }
    }
    if body_ids != resource_ids {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Bilibili task output resource has no matching body",
        ));
    }
    Ok(())
}

fn create_staged_resource_bodies(
    staged: &StagedTaskOutputReplacement<'_>,
    bodies: &[BilibiliTaskResourceBody],
) -> io::Result<()> {
    let bodies = bodies
        .iter()
        .map(|body| (body.resource_id.as_str(), &body.source))
        .collect::<HashMap<_, _>>();
    for resource in staged.resources_requiring_body_creation() {
        let source = bodies.get(resource.resource.id.as_str()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Bilibili task output resource has no matching body",
            )
        })?;
        match source {
            BilibiliTaskResourceBodySource::CachePath(path) => {
                staged.copy_resource_body_from_cache_path(&resource.resource.id, path)?;
            }
            BilibiliTaskResourceBodySource::Bytes(bytes) => {
                staged.write_resource_body(&resource.resource.id, bytes)?;
            }
        }
    }
    Ok(())
}

async fn complete_terminal_task<F>(registry: &Arc<BilibiliTaskRegistry>, complete: F)
where
    F: Fn(&BilibiliTaskRegistry) -> Result<Task, tonic::Status> + Send + Sync + 'static,
{
    let _ = complete_terminal_task_with_outcome(registry, complete).await;
}

async fn complete_terminal_task_with_outcome<F>(
    registry: &Arc<BilibiliTaskRegistry>,
    complete: F,
) -> Result<(), tonic::Status>
where
    F: Fn(&BilibiliTaskRegistry) -> Result<Task, tonic::Status> + Send + Sync + 'static,
{
    let complete = Arc::new(complete);
    loop {
        let completion_registry = Arc::clone(registry);
        let completion = Arc::clone(&complete);
        let Some(attempt) = run_blocking_terminal_persistence_attempt(
            "Bilibili terminal task completion",
            move || completion(&completion_registry),
        )
        .await
        else {
            return Err(tonic::Status::internal(
                "Bilibili terminal task completion could not be joined.",
            ));
        };
        match attempt {
            Ok(_)
                if !registry.persistence_recovery_supported()
                    || registry.persistence_available() =>
            {
                return Ok(());
            }
            Ok(_) => {}
            Err(error) if error.code() == tonic::Code::Unavailable => {}
            Err(error) => return Err(error),
        }
        loop {
            match retry_pending_task_persistence(registry).await {
                TaskPersistenceRecoveryOutcome::Durable => break,
                TaskPersistenceRecoveryOutcome::RetryableFailure => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                TaskPersistenceRecoveryOutcome::PermanentFailure => {
                    eprintln!(
                        "Bilibili task persistence recovery was rejected permanently; releasing the worker slot"
                    );
                    return Err(tonic::Status::unavailable(
                        "Bilibili task persistence recovery was rejected permanently.",
                    ));
                }
            }
        }
    }
}

async fn retry_pending_task_persistence(
    registry: &Arc<BilibiliTaskRegistry>,
) -> TaskPersistenceRecoveryOutcome {
    let registry = Arc::clone(registry);
    run_blocking_terminal_persistence_attempt("Bilibili task persistence retry", move || {
        registry.retry_pending_persistence_outcome()
    })
    .await
    .unwrap_or(TaskPersistenceRecoveryOutcome::PermanentFailure)
}

async fn run_blocking_terminal_persistence_attempt<T>(
    context: &'static str,
    attempt: impl FnOnce() -> T + Send + 'static,
) -> Option<T>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(attempt).await {
        Ok(result) => Some(result),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => {
            eprintln!("Failed to join {context}: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use crate::{
        bilibili_playback::{BilibiliContentIdentity, BilibiliContentKind},
        generated::tvos_net_player::v1::{
            CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState, TaskProblem,
            TaskProblemCategory, TaskResult,
        },
        task_registry::TaskRetentionPolicy,
    };
    use tokio::sync::Notify;

    use super::*;

    #[tokio::test]
    async fn worker_reports_progress_and_marks_task_succeeded() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::new(SuccessAdapter),
            1,
            false,
        ));
        let task = registry
            .create_bilibili_task("BV1success", None)
            .expect("task should be created");

        let completed = wait_for_state(&registry, &task.id, TaskState::Succeeded).await;

        worker.abort();
        assert_eq!("local.default.sample", completed.library_item_id);
        assert_eq!("Downloaded into the cache library.", completed.message);
        assert_eq!(1.0, completed.progress);
        assert_eq!(512, completed.downloaded_bytes);
        assert_eq!(1024, completed.total_bytes);
    }

    #[tokio::test]
    async fn worker_marks_adapter_failure() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::new(FailureAdapter),
            1,
            false,
        ));
        let task = registry
            .create_bilibili_task("BV1failure", None)
            .expect("task should be created");

        let completed = wait_for_state(&registry, &task.id, TaskState::Failed).await;

        worker.abort();
        assert_eq!("adapter failed", completed.message);
    }

    #[tokio::test]
    async fn worker_publishes_partial_v2_results_resources_and_redacted_errors_durably() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("state").join("tasks.json");
        let resource_root = temp.path().join("library");
        std::fs::create_dir_all(&resource_root).expect("resource root should be created");
        let registry = Arc::new(
            BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
                &state_path,
                TaskRetentionPolicy::default(),
                Some(resource_root.clone()),
            ),
        );
        let candidates = vec![test_candidate(1), test_candidate(2)];
        let task = registry
            .create_bilibili_download_task_v2(
                "BV1worker-v2",
                None,
                None,
                "Worker v2".to_owned(),
                candidates,
            )
            .expect("v2 task should be created durably");
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::new(PartialV2Adapter),
            1,
            true,
        ));

        let completed = wait_for_state(&registry, &task.id, TaskState::Succeeded).await;
        worker.abort();
        let _ = worker.await;

        assert_eq!("local.default.worker-v2", completed.library_item_id);
        assert_eq!(2, completed.result_items.len());
        assert_eq!(TaskState::Succeeded, completed.result_items[0].state());
        assert_eq!(TaskState::Failed, completed.result_items[1].state());
        assert!(
            !completed.result_items[1]
                .message
                .contains("sensitive-marker")
        );

        let snapshot = registry
            .task_output_snapshot(&task.id)
            .expect("v2 output should be visible");
        assert_eq!(2, snapshot.output.record.results.len());
        assert_eq!(1, snapshot.output.record.resources.len());
        let failed = &snapshot.output.record.results[1];
        assert_eq!(TaskState::Failed, failed.state());
        assert!(
            failed
                .problem
                .as_ref()
                .is_some_and(|problem| problem.message.contains("detail omitted"))
        );
        assert!(
            !failed
                .problem
                .as_ref()
                .is_some_and(|problem| problem.message.contains("sensitive-marker"))
        );
        let resource = &snapshot.output.record.resources[0];
        let resource_path = resource_root.join(resource.relative_path());
        assert_eq!(
            b"worker subtitle\n".to_vec(),
            std::fs::read(&resource_path).expect("resource body should be durable")
        );
        drop(snapshot);
        drop(registry);

        let restored = BilibiliTaskRegistry::with_persistence_path_retention_and_resource_root(
            state_path,
            TaskRetentionPolicy::default(),
            Some(resource_root),
        );
        let restored_task = restored
            .get_task(&task.id)
            .expect("terminal task should survive restart");
        let restored_output = restored
            .task_output_snapshot(&task.id)
            .expect("v2 output should survive restart");
        assert_eq!(TaskState::Succeeded, restored_task.state());
        assert_eq!(2, restored_output.output.record.results.len());
        assert_eq!(1, restored_output.output.record.resources.len());
    }

    #[tokio::test]
    async fn worker_does_not_start_v2_download_until_creation_is_durable() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("state").join("tasks.json");
        let registry = Arc::new(BilibiliTaskRegistry::with_persistence_path(&state_path));
        registry.fail_next_persistence_directory_sync();
        let task = registry
            .create_bilibili_download_task_v2(
                "BV1worker-creation-durability-v2",
                None,
                None,
                "Worker durability".to_owned(),
                vec![test_candidate(1)],
            )
            .expect("the installed v2 download should remain queued");
        assert!(!registry.persistence_available());

        std::fs::remove_file(&state_path).expect("installed state should be removable");
        std::fs::create_dir(&state_path).expect("directory should keep persistence unavailable");
        let adapter = Arc::new(StartCountingAdapter::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::clone(&adapter) as Arc<dyn BilibiliDownloadAdapter>,
            1,
            false,
        ));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(0, adapter.start_count.load(Ordering::SeqCst));
        assert_eq!(
            TaskState::Queued,
            registry.get_task(&task.id).unwrap().state()
        );

        std::fs::remove_dir(&state_path).expect("blocking directory should be removable");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let started = adapter.started.notified();
                if adapter.start_count.load(Ordering::SeqCst) == 1 {
                    break;
                }
                started.await;
            }
        })
        .await
        .expect("the adapter should start after persistence recovers");
        let completed = wait_for_state(&registry, &task.id, TaskState::Succeeded).await;

        worker.abort();
        let _ = worker.await;
        assert_eq!(TaskState::Succeeded, completed.state());
        assert!(registry.persistence_available());
    }

    #[tokio::test]
    async fn terminal_completion_returns_immediately_without_configured_persistence() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempt_count = Arc::clone(&attempts);

        tokio::time::timeout(
            Duration::from_secs(1),
            complete_terminal_task(&registry, move |_| {
                attempt_count.fetch_add(1, Ordering::SeqCst);
                Ok(Default::default())
            }),
        )
        .await
        .expect("unconfigured persistence should not delay terminal completion");

        assert_eq!(1, attempts.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_completion_attempt_does_not_block_the_async_runtime() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let attempt_started = Arc::new(AtomicBool::new(false));
        let runtime_progressed = Arc::new(AtomicBool::new(false));
        let release = Arc::new((Mutex::new(false), Condvar::new()));

        let observer_started = Arc::clone(&attempt_started);
        let observer_progressed = Arc::clone(&runtime_progressed);
        let observer_release = Arc::clone(&release);
        let observer = std::thread::spawn(move || {
            let start_deadline = Instant::now() + Duration::from_secs(2);
            while !observer_started.load(Ordering::SeqCst) && Instant::now() < start_deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let started_before_deadline = observer_started.load(Ordering::SeqCst);

            let progress_deadline = Instant::now() + Duration::from_millis(500);
            while started_before_deadline
                && !observer_progressed.load(Ordering::SeqCst)
                && Instant::now() < progress_deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            let progressed_before_release = observer_progressed.load(Ordering::SeqCst);

            let (released, release_signal) = &*observer_release;
            *released
                .lock()
                .expect("release lock should not be poisoned") = true;
            release_signal.notify_one();
            (started_before_deadline, progressed_before_release)
        });

        let heartbeat_progressed = Arc::clone(&runtime_progressed);
        let heartbeat = tokio::spawn(async move {
            tokio::task::yield_now().await;
            heartbeat_progressed.store(true, Ordering::SeqCst);
        });
        let completion_started = Arc::clone(&attempt_started);
        let completion_release = Arc::clone(&release);
        complete_terminal_task(&registry, move |_| {
            completion_started.store(true, Ordering::SeqCst);
            let (released, release_signal) = &*completion_release;
            let mut released = released
                .lock()
                .expect("release lock should not be poisoned");
            while !*released {
                released = release_signal
                    .wait(released)
                    .expect("release lock should not be poisoned");
            }
            Ok(Default::default())
        })
        .await;

        heartbeat.await.expect("heartbeat should finish cleanly");
        let (started_before_deadline, progressed_before_release) =
            observer.join().expect("observer should finish cleanly");
        assert!(started_before_deadline, "the blocking attempt should start");
        assert!(
            progressed_before_release,
            "the async runtime should progress while terminal persistence is blocked"
        );
    }

    #[tokio::test]
    async fn aborting_terminal_completion_does_not_repeat_an_in_flight_attempt() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let attempts = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));

        let completion_registry = Arc::clone(&registry);
        let completion_attempts = Arc::clone(&attempts);
        let completion_started = Arc::clone(&started);
        let completion_finished = Arc::clone(&finished);
        let completion_release = Arc::clone(&release);
        let started_wait = started.notified();
        let completion = tokio::spawn(async move {
            complete_terminal_task(&completion_registry, move |_| {
                completion_attempts.fetch_add(1, Ordering::SeqCst);
                completion_started.notify_one();
                let (released, release_signal) = &*completion_release;
                let mut released = released
                    .lock()
                    .expect("release lock should not be poisoned");
                while !*released {
                    released = release_signal
                        .wait(released)
                        .expect("release lock should not be poisoned");
                }
                completion_finished.notify_one();
                Err(tonic::Status::unavailable(
                    "persistence remains unavailable",
                ))
            })
            .await;
        });

        tokio::time::timeout(Duration::from_secs(1), started_wait)
            .await
            .expect("the blocking attempt should start");
        completion.abort();
        let finished_wait = finished.notified();
        let (released, release_signal) = &*release;
        *released
            .lock()
            .expect("release lock should not be poisoned") = true;
        release_signal.notify_one();
        tokio::time::timeout(Duration::from_secs(1), finished_wait)
            .await
            .expect("the in-flight blocking attempt should finish after cancellation");
        assert!(
            completion
                .await
                .expect_err("the async owner should remain cancelled")
                .is_cancelled()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(1, attempts.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn terminal_completion_returns_when_configured_persistence_cannot_retry() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("tasks.json");
        std::fs::write(&path, b"{ invalid task state")
            .expect("invalid task state should be written");
        let registry = Arc::new(BilibiliTaskRegistry::with_persistence_path(&path));
        let task = registry
            .create_bilibili_task("BV1detached-persistence", None)
            .expect("registry should remain usable in memory");
        let work_item = registry
            .try_claim_next_bilibili_task()
            .expect("volatile task should start running");
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempt_count = Arc::clone(&attempts);

        tokio::time::timeout(
            Duration::from_secs(1),
            complete_terminal_task(&registry, move |registry| {
                attempt_count.fetch_add(1, Ordering::SeqCst);
                registry.complete_task_succeeded(
                    &work_item.task_id,
                    "local.default.sample".to_owned(),
                    "Downloaded into the volatile cache library.".to_owned(),
                )
            }),
        )
        .await
        .expect("a detached malformed store cannot recover before restart");

        assert_eq!(1, attempts.load(Ordering::SeqCst));
        assert!(registry.persistence_configured());
        assert!(!registry.persistence_recovery_supported());
        assert!(!registry.persistence_available());
        assert_eq!(
            TaskState::Succeeded,
            registry.get_task(&task.id).unwrap().state()
        );
    }

    #[tokio::test]
    async fn terminal_completion_does_not_retry_non_retryable_errors() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = Arc::new(BilibiliTaskRegistry::with_persistence_path(&path));
        registry.fail_next_persistence_directory_sync();
        registry
            .create_bilibili_task("BV1non-retryable", None)
            .expect("task should be installed before directory sync fails");
        assert!(!registry.persistence_available());
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempt_count = Arc::clone(&attempts);

        tokio::time::timeout(
            Duration::from_secs(1),
            complete_terminal_task(&registry, move |_| {
                attempt_count.fetch_add(1, Ordering::SeqCst);
                Err(tonic::Status::failed_precondition(
                    "terminal transition is not allowed",
                ))
            }),
        )
        .await
        .expect("non-retryable completion errors should return immediately");

        assert_eq!(1, attempts.load(Ordering::SeqCst));
        assert!(!registry.persistence_available());
    }

    #[tokio::test]
    async fn permanent_persistence_rejection_releases_the_worker_permit() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = Arc::new(BilibiliTaskRegistry::with_persistence_path(&path));
        let creation = registry
            .create_bilibili_playback_task("BV1permanent-persistence", None, None)
            .expect("playback task should be created durably");
        registry
            .create_bilibili_task("BV1queued-one", None)
            .expect("first download should be queued durably");
        registry
            .create_bilibili_task("BV1queued-two", None)
            .expect("second download should be queued durably");
        registry.inject_permanently_invalid_playback_result_for_test(&creation.task.id);
        assert_eq!(
            TaskPersistenceRecoveryOutcome::PermanentFailure,
            registry.retry_pending_persistence_outcome()
        );
        assert!(!registry.persistence_available());
        let adapter = Arc::new(StartCountingAdapter::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::clone(&adapter) as Arc<dyn BilibiliDownloadAdapter>,
            1,
            false,
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let started = adapter.started.notified();
                if adapter.start_count.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                started.await;
            }
        })
        .await
        .expect("the second task should start after the first permanent rejection");

        worker.abort();
        let _ = worker.await;
        assert_eq!(2, adapter.start_count.load(Ordering::SeqCst));
        assert_eq!(
            TaskPersistenceRecoveryOutcome::PermanentFailure,
            registry.retry_pending_persistence_outcome()
        );
    }

    #[tokio::test]
    async fn terminal_completion_keeps_worker_ownership_until_directory_sync_repair() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = Arc::new(BilibiliTaskRegistry::with_persistence_path(&path));
        let task = registry
            .create_bilibili_task("BV1worker-directory-sync", None)
            .expect("task should be created durably");
        let work_item = registry
            .try_claim_next_bilibili_task()
            .expect("task should start running");
        registry.fail_next_persistence_directory_sync();

        let completion_registry = Arc::clone(&registry);
        let completion_path = path.clone();
        let installed = Arc::new(Notify::new());
        let installed_signal = Arc::clone(&installed);
        let first_attempt = Arc::new(AtomicBool::new(true));
        let completion = tokio::spawn(async move {
            complete_terminal_task(&completion_registry, move |registry| {
                let result = registry.complete_task_succeeded(
                    &work_item.task_id,
                    "local.default.sample".to_owned(),
                    "Downloaded into the cache library.".to_owned(),
                );
                if first_attempt.swap(false, Ordering::SeqCst) {
                    assert!(result.is_ok());
                    assert!(!registry.persistence_available());
                    std::fs::remove_file(&completion_path)
                        .expect("installed snapshot should be removable");
                    std::fs::create_dir(&completion_path)
                        .expect("directory should keep persistence unavailable");
                    installed_signal.notify_one();
                }
                result
            })
            .await;
        });

        tokio::time::timeout(Duration::from_secs(1), installed.notified())
            .await
            .expect("terminal state should be installed before directory sync fails");
        assert!(!completion.is_finished());
        assert!(!registry.persistence_available());
        assert_eq!(
            TaskState::Succeeded,
            registry.get_task(&task.id).unwrap().state()
        );

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        tokio::time::timeout(Duration::from_secs(3), completion)
            .await
            .expect("worker should retry after directory repair")
            .expect("worker task should finish cleanly");

        assert!(registry.persistence_available());
        drop(registry);
        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        assert_eq!(
            TaskState::Succeeded,
            restored.get_task(&task.id).unwrap().state()
        );
    }

    #[tokio::test]
    async fn worker_waits_for_terminal_state_persistence_to_recover() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("state").join("tasks.json");
        let registry = Arc::new(BilibiliTaskRegistry::with_persistence_path(&path));
        let task = registry
            .create_bilibili_task("BV1worker-persistence", None)
            .expect("task should be created durably");
        let work_item = registry
            .try_claim_next_bilibili_task()
            .expect("task should start running");

        std::fs::remove_file(&path).expect("task state should be removable");
        std::fs::create_dir(&path).expect("directory should block snapshot replacement");
        let completion_registry = Arc::clone(&registry);
        let completion = tokio::spawn(run_one_bilibili_task(
            completion_registry,
            Arc::new(SuccessAdapter),
            work_item,
            false,
        ));
        for _ in 0..100 {
            if !registry.persistence_available() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(!registry.persistence_available());
        assert!(!completion.is_finished());
        assert_eq!(
            TaskState::Running,
            registry.get_task(&task.id).unwrap().state()
        );

        std::fs::remove_dir(&path).expect("blocking directory should be removable");
        tokio::time::timeout(Duration::from_secs(3), completion)
            .await
            .expect("worker should retry after persistence recovers")
            .expect("worker task should finish cleanly");

        let completed = registry.get_task(&task.id).unwrap();
        assert_eq!(TaskState::Succeeded, completed.state());
        assert!(registry.persistence_available());
        drop(registry);
        let restored = BilibiliTaskRegistry::with_persistence_path(&path);
        assert_eq!(
            TaskState::Succeeded,
            restored.get_task(&task.id).unwrap().state()
        );
    }

    #[tokio::test]
    async fn worker_omits_adapter_failure_detail_when_credentials_are_configured() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::new(FailureAdapter),
            1,
            true,
        ));
        let task = registry
            .create_bilibili_task("BV1credential-failure", None)
            .expect("task should be created");

        let completed = wait_for_state(&registry, &task.id, TaskState::Failed).await;

        worker.abort();
        assert_eq!(
            crate::credential_safe_client_error(true, &"adapter failed"),
            completed.message
        );
        assert!(!completed.message.contains("adapter failed"));
    }

    #[tokio::test]
    async fn worker_marks_adapter_panic_as_failure_and_allows_requeue() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::new(PanicAdapter),
            1,
            false,
        ));
        let task = registry
            .create_bilibili_task("BV1panic", None)
            .expect("task should be created");

        let completed = wait_for_state(&registry, &task.id, TaskState::Failed).await;
        let requeued = registry
            .create_bilibili_task("BV1panic", None)
            .expect("failed source should be requeueable");

        worker.abort();
        assert_eq!("Bilibili download adapter panicked.", completed.message);
        assert_ne!(task.id, requeued.id);
    }

    #[tokio::test]
    async fn worker_abort_drops_running_adapter_future() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let adapter = Arc::new(DropAwareAdapter::default());
        let adapter_started = adapter.started.notified();
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::clone(&adapter) as Arc<dyn BilibiliDownloadAdapter>,
            1,
            false,
        ));
        let task = registry
            .create_bilibili_task("BV1drop", None)
            .expect("task should be created");

        tokio::time::timeout(Duration::from_secs(1), adapter_started)
            .await
            .expect("adapter should start");
        let running = registry.get_task(&task.id).expect("task should exist");
        assert_eq!(TaskState::Running, running.state());

        worker.abort();
        let _ = worker.await;
        tokio::time::timeout(Duration::from_secs(1), adapter.dropped.notified())
            .await
            .expect("adapter future should be dropped");
    }

    #[tokio::test]
    async fn worker_exposes_running_cancellation_to_adapter() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::new(CancellationAwareAdapter),
            1,
            false,
        ));
        let task = registry
            .create_bilibili_task("BV1cancel", None)
            .expect("task should be created");
        let _ = wait_for_state(&registry, &task.id, TaskState::Running).await;

        let cancel_requested = registry.cancel_task(&task.id).expect("cancel should work");
        assert_eq!(TaskState::CancelRequested, cancel_requested.state());
        let completed = wait_for_state(&registry, &task.id, TaskState::Cancelled).await;

        worker.abort();
        assert_eq!("adapter observed cancellation", completed.message);
    }

    struct SuccessAdapter;

    struct PartialV2Adapter;

    impl BilibiliDownloadAdapter for PartialV2Adapter {
        fn run<'a>(
            &'a self,
            request: BilibiliDownloadRequest,
            _context: BilibiliDownloadContext,
        ) -> BilibiliDownloadFuture<'a> {
            Box::pin(async move {
                let body = b"worker subtitle\n".to_vec();
                let resource = TaskResourceRecord::new(CacheResourceRef {
                    id: "worker-v2-subtitle".to_owned(),
                    content_type: "text/vtt; charset=utf-8".to_owned(),
                    size_bytes: i64::try_from(body.len()).expect("test body should fit in i64"),
                    size_known: true,
                    ..Default::default()
                })
                .expect("test resource should be valid");
                let successful_result = TaskResult {
                    id: request.task_id.clone(),
                    state: TaskState::Succeeded.into(),
                    title: request.candidates[0].title.clone(),
                    library_item_id: "local.default.worker-v2".to_owned(),
                    artifacts: vec![
                        TaskArtifact {
                            id: "worker-v2-media".to_owned(),
                            kind: TaskArtifactKind::Media.into(),
                            state: TaskArtifactState::Available.into(),
                            library_item_id: "local.default.worker-v2".to_owned(),
                            ..Default::default()
                        },
                        TaskArtifact {
                            id: "worker-v2-subtitle".to_owned(),
                            kind: TaskArtifactKind::Subtitle.into(),
                            state: TaskArtifactState::Available.into(),
                            resource: Some(resource.resource.clone()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                };
                let failed_result = TaskResult {
                    id: format!("{}-result-2", request.task_id),
                    state: TaskState::Failed.into(),
                    title: request.candidates[1].title.clone(),
                    problem: Some(TaskProblem {
                        category: TaskProblemCategory::Upstream.into(),
                        code: "bilibili.download_failed".to_owned(),
                        message: "upstream sensitive-marker".to_owned(),
                        retryable: true,
                    }),
                    ..Default::default()
                };
                Ok(BilibiliDownloadOutput {
                    library_item_id: "local.default.worker-v2".to_owned(),
                    message: "Downloaded 1/2 Bilibili result(s).".to_owned(),
                    v2: Some(BilibiliDownloadOutputV2 {
                        terminal_state: TaskState::Succeeded,
                        results: vec![successful_result, failed_result],
                        resources: vec![resource],
                        resource_bodies: vec![BilibiliTaskResourceBody {
                            resource_id: "worker-v2-subtitle".to_owned(),
                            source: BilibiliTaskResourceBodySource::Bytes(body),
                        }],
                    }),
                })
            })
        }
    }

    impl BilibiliDownloadAdapter for SuccessAdapter {
        fn run<'a>(
            &'a self,
            _request: BilibiliDownloadRequest,
            context: BilibiliDownloadContext,
        ) -> BilibiliDownloadFuture<'a> {
            Box::pin(async move {
                context.report_progress(BilibiliTaskProgress {
                    progress: Some(0.5),
                    downloaded_bytes: Some(512),
                    total_bytes: Some(1024),
                    message: Some("Downloading media.".to_owned()),
                });
                Ok(BilibiliDownloadOutput {
                    library_item_id: "local.default.sample".to_owned(),
                    message: "Downloaded into the cache library.".to_owned(),
                    v2: None,
                })
            })
        }
    }

    fn test_candidate(index: u32) -> BilibiliTaskCandidateRecord {
        BilibiliTaskCandidateRecord {
            selection_id: format!("page:{index}:cid:{}", 2_000 + index),
            title: format!("Part {index}"),
            subtitle: format!("Page {index}"),
            source_kind: "video_page".to_owned(),
            content_id: (2_000 + index).to_string(),
            identity: BilibiliContentIdentity {
                kind: BilibiliContentKind::VideoPage,
                aid: Some(1_001),
                bvid: Some("BV1worker-v2".to_owned()),
                cid: Some(u64::from(2_000 + index)),
                epid: None,
            },
            index,
            duration_seconds: Some(60),
        }
    }

    #[derive(Default)]
    struct StartCountingAdapter {
        start_count: AtomicUsize,
        started: Notify,
    }

    impl BilibiliDownloadAdapter for StartCountingAdapter {
        fn run<'a>(
            &'a self,
            _request: BilibiliDownloadRequest,
            _context: BilibiliDownloadContext,
        ) -> BilibiliDownloadFuture<'a> {
            self.start_count.fetch_add(1, Ordering::SeqCst);
            self.started.notify_waiters();
            Box::pin(async {
                Ok(BilibiliDownloadOutput {
                    library_item_id: "local.default.sample".to_owned(),
                    message: "Downloaded into the cache library.".to_owned(),
                    v2: None,
                })
            })
        }
    }

    struct FailureAdapter;

    impl BilibiliDownloadAdapter for FailureAdapter {
        fn run<'a>(
            &'a self,
            _request: BilibiliDownloadRequest,
            _context: BilibiliDownloadContext,
        ) -> BilibiliDownloadFuture<'a> {
            Box::pin(async { Err(BilibiliDownloadError::Failed("adapter failed".to_owned())) })
        }
    }

    struct PanicAdapter;

    impl BilibiliDownloadAdapter for PanicAdapter {
        fn run<'a>(
            &'a self,
            _request: BilibiliDownloadRequest,
            _context: BilibiliDownloadContext,
        ) -> BilibiliDownloadFuture<'a> {
            panic!("adapter panic")
        }
    }

    #[derive(Default)]
    struct DropAwareAdapter {
        started: Arc<Notify>,
        dropped: Arc<Notify>,
    }

    impl BilibiliDownloadAdapter for DropAwareAdapter {
        fn run<'a>(
            &'a self,
            _request: BilibiliDownloadRequest,
            _context: BilibiliDownloadContext,
        ) -> BilibiliDownloadFuture<'a> {
            let started = Arc::clone(&self.started);
            let dropped = Arc::clone(&self.dropped);
            Box::pin(async move {
                let _drop_guard = DropNotifier { notify: dropped };
                started.notify_one();
                std::future::pending().await
            })
        }
    }

    struct DropNotifier {
        notify: Arc<Notify>,
    }

    impl Drop for DropNotifier {
        fn drop(&mut self) {
            self.notify.notify_one();
        }
    }

    struct CancellationAwareAdapter;

    impl BilibiliDownloadAdapter for CancellationAwareAdapter {
        fn run<'a>(
            &'a self,
            _request: BilibiliDownloadRequest,
            context: BilibiliDownloadContext,
        ) -> BilibiliDownloadFuture<'a> {
            Box::pin(async move {
                loop {
                    if context.is_cancel_requested() {
                        return Err(BilibiliDownloadError::Cancelled(
                            "adapter observed cancellation".to_owned(),
                        ));
                    }

                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
        }
    }

    async fn wait_for_state(
        registry: &BilibiliTaskRegistry,
        task_id: &str,
        expected_state: TaskState,
    ) -> crate::generated::tvos_net_player::v1::Task {
        for _ in 0..100 {
            let task = registry.get_task(task_id).expect("task should exist");
            if task.state() == expected_state {
                return task;
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        panic!("task did not reach expected state");
    }
}
