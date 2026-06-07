use std::{future::Future, panic::AssertUnwindSafe, pin::Pin, sync::Arc};

use futures_util::FutureExt;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{
    generated::tvos_net_player::v1::BilibiliDownloadOptions,
    task_registry::{
        BilibiliTaskCancellation, BilibiliTaskProgress, BilibiliTaskRegistry, BilibiliTaskWorkItem,
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
}

pub struct BilibiliDownloadOutput {
    pub library_item_id: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BilibiliDownloadError {
    Failed(String),
    Cancelled(String),
}

impl BilibiliDownloadError {
    fn message(self) -> String {
        match self {
            Self::Failed(message) | Self::Cancelled(message) => message,
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
        let work_item = registry.claim_next_bilibili_task().await;
        let registry = Arc::clone(&registry);
        let adapter = Arc::clone(&adapter);
        running_tasks.spawn(async move {
            let _permit = permit;
            run_one_bilibili_task(registry, adapter, work_item).await;
        });
    }
}

async fn run_one_bilibili_task(
    registry: Arc<BilibiliTaskRegistry>,
    adapter: Arc<dyn BilibiliDownloadAdapter>,
    work_item: BilibiliTaskWorkItem,
) {
    let request = BilibiliDownloadRequest {
        task_id: work_item.task_id.clone(),
        source: work_item.source,
        options: work_item.options,
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
            let _ = registry.complete_task_failed(
                &work_item.task_id,
                "Bilibili download adapter panicked.".to_owned(),
            );
            return;
        }
    };

    if work_item.cancellation.is_cancel_requested() {
        let message = match result {
            Err(BilibiliDownloadError::Cancelled(message)) => message,
            _ => "Cancelled by request.".to_owned(),
        };
        let _ = registry.complete_task_cancelled(&work_item.task_id, message);
        return;
    }

    match result {
        Ok(output) => {
            let _ = registry.complete_task_succeeded(
                &work_item.task_id,
                output.library_item_id,
                output.message,
            );
        }
        Err(BilibiliDownloadError::Cancelled(message)) => {
            let _ = registry.complete_task_cancelled(&work_item.task_id, message);
        }
        Err(error @ BilibiliDownloadError::Failed(_)) => {
            let _ = registry.complete_task_failed(&work_item.task_id, error.message());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::generated::tvos_net_player::v1::TaskState;
    use tokio::sync::Notify;

    use super::*;

    #[tokio::test]
    async fn worker_reports_progress_and_marks_task_succeeded() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::new(SuccessAdapter),
            1,
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
        ));
        let task = registry
            .create_bilibili_task("BV1failure", None)
            .expect("task should be created");

        let completed = wait_for_state(&registry, &task.id, TaskState::Failed).await;

        worker.abort();
        assert_eq!("adapter failed", completed.message);
    }

    #[tokio::test]
    async fn worker_marks_adapter_panic_as_failure_and_allows_requeue() {
        let registry = Arc::new(BilibiliTaskRegistry::default());
        let worker = tokio::spawn(run_bilibili_task_worker(
            Arc::clone(&registry),
            Arc::new(PanicAdapter),
            1,
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
