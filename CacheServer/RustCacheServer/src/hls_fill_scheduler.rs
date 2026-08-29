use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::Notify;

use crate::{grpc_services::HlsCacheFinalizationFailureMode, hls::HlsPlaybackSession};

#[derive(Clone, Default)]
pub(crate) struct HlsFillScheduler {
    inner: Arc<Mutex<HlsFillSchedulerInner>>,
    notify: Arc<Notify>,
    current_finished: Arc<Notify>,
}

#[derive(Default)]
struct HlsFillSchedulerInner {
    foreground: VecDeque<HlsFillJob>,
    demoted: Vec<HlsFillJob>,
    current: Option<HlsFillCurrentJob>,
    worker_started: bool,
    closed: bool,
    next_sequence: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct HlsFillJob {
    pub(crate) task_id: String,
    pub(crate) session: HlsPlaybackSession,
    pub(crate) failure_mode: HlsCacheFinalizationFailureMode,
    pub(crate) priority: HlsFillPriority,
    pub(crate) token: HlsFillPreemptionToken,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsFillPriority {
    Foreground,
    Demoted,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HlsFillPreemptionToken {
    preempted: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

struct HlsFillCurrentJob {
    job: HlsFillJob,
}

pub(crate) struct HlsFillWorkerGuard {
    scheduler: HlsFillScheduler,
}

impl Drop for HlsFillWorkerGuard {
    fn drop(&mut self) {
        self.scheduler.mark_worker_stopped();
    }
}

impl HlsFillScheduler {
    pub(crate) fn enqueue_foreground(
        &self,
        task_id: String,
        session: HlsPlaybackSession,
        failure_mode: HlsCacheFinalizationFailureMode,
    ) -> bool {
        let should_start_worker = self.enqueue(
            task_id,
            session,
            failure_mode,
            HlsFillPriority::Foreground,
            true,
        );
        self.notify.notify_one();
        should_start_worker
    }

    pub(crate) fn enqueue_demoted(
        &self,
        task_id: String,
        session: HlsPlaybackSession,
        failure_mode: HlsCacheFinalizationFailureMode,
    ) -> bool {
        let should_start_worker = self.enqueue(
            task_id,
            session,
            failure_mode,
            HlsFillPriority::Demoted,
            false,
        );
        self.notify.notify_one();
        should_start_worker
    }

    #[cfg(test)]
    pub(crate) async fn next_job(&self) -> HlsFillJob {
        self.next_job_until_shutdown()
            .await
            .expect("HLS fill scheduler shut down while a test was awaiting work")
    }

    pub(crate) async fn next_job_until_shutdown(&self) -> Option<HlsFillJob> {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
                if inner.closed {
                    return None;
                }
                if let Some(job) = inner.foreground.pop_back().or_else(|| inner.demoted.pop()) {
                    inner.current = Some(HlsFillCurrentJob { job: job.clone() });
                    return Some(job);
                }
            }
            notified.await;
        }
    }

    pub(crate) fn finish_current(&self, job: &HlsFillJob, requeue_preempted: bool) {
        let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        let is_current = inner.current.as_ref().is_some_and(|current| {
            current.job.sequence == job.sequence && current.job.task_id == job.task_id
        });
        if !is_current {
            return;
        }
        let current = inner
            .current
            .take()
            .expect("matched HLS fill current job should exist");
        let should_requeue = requeue_preempted
            && !current.job.token.is_cancelled()
            && !inner.closed
            && !inner.has_queued_session(&current.job.session.id);
        if should_requeue {
            let job = inner.refresh_job(current.job, HlsFillPriority::Demoted);
            inner.demoted.push(job);
        }
        drop(inner);
        self.current_finished.notify_waiters();
        if should_requeue {
            self.notify.notify_one();
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        let inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        inner.current.is_none() && inner.foreground.is_empty() && inner.demoted.is_empty()
    }

    pub(crate) fn owns_session(&self, session_id: &str) -> bool {
        let inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        inner
            .current
            .as_ref()
            .is_some_and(|current| current.job.session.id == session_id)
            || inner.has_queued_session(session_id)
    }

    pub(crate) fn cancel_task(&self, task_id: &str) {
        let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        inner.foreground.retain(|job| job.task_id != task_id);
        inner.demoted.retain(|job| job.task_id != task_id);
        if let Some(current) = inner.current.as_ref()
            && current.job.task_id == task_id
        {
            current.job.token.cancel();
        }
    }

    pub(crate) fn cancel_sessions(&self, session_ids: &HashSet<String>) -> bool {
        let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        inner
            .foreground
            .retain(|job| !session_ids.contains(&job.session.id));
        inner
            .demoted
            .retain(|job| !session_ids.contains(&job.session.id));
        let matching_current = inner.current.as_ref().is_some_and(|current| {
            if !session_ids.contains(&current.job.session.id) {
                return false;
            }
            current.job.token.cancel();
            true
        });
        !matching_current || !inner.worker_started
    }

    pub(crate) async fn cancel_task_and_wait(&self, task_id: &str) {
        loop {
            let current_finished = self.current_finished.notified();
            tokio::pin!(current_finished);
            current_finished.as_mut().enable();
            let should_wait = {
                let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
                inner.foreground.retain(|job| job.task_id != task_id);
                inner.demoted.retain(|job| job.task_id != task_id);
                inner.current.as_ref().is_some_and(|current| {
                    if current.job.task_id != task_id {
                        return false;
                    }
                    current.job.token.cancel();
                    inner.worker_started
                })
            };
            if !should_wait {
                return;
            }
            current_finished.await;
        }
    }

    pub(crate) async fn cancel_sessions_and_wait(&self, session_ids: &HashSet<String>) {
        loop {
            let current_finished = self.current_finished.notified();
            tokio::pin!(current_finished);
            current_finished.as_mut().enable();
            let should_wait = {
                let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
                inner
                    .foreground
                    .retain(|job| !session_ids.contains(&job.session.id));
                inner
                    .demoted
                    .retain(|job| !session_ids.contains(&job.session.id));
                inner.current.as_ref().is_some_and(|current| {
                    if !session_ids.contains(&current.job.session.id) {
                        return false;
                    }
                    current.job.token.cancel();
                    inner.worker_started
                })
            };
            if !should_wait {
                return;
            }
            current_finished.await;
        }
    }

    pub(crate) async fn shutdown_and_wait_for_worker(&self) {
        {
            let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
            inner.closed = true;
            inner.foreground.clear();
            inner.demoted.clear();
            if let Some(current) = inner.current.as_ref() {
                current.job.token.cancel();
            }
        }
        self.notify.notify_waiters();

        loop {
            let notified = self.notify.notified();
            if !self
                .inner
                .lock()
                .expect("HLS fill scheduler lock poisoned")
                .worker_started
            {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn worker_guard(&self) -> HlsFillWorkerGuard {
        HlsFillWorkerGuard {
            scheduler: self.clone(),
        }
    }

    pub(crate) fn diagnostic_counts(&self) -> (bool, usize, usize) {
        let inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        (
            inner.current.is_some(),
            inner.foreground.len(),
            inner.demoted.len(),
        )
    }

    pub(crate) fn promote_session_to_foreground(
        &self,
        session_id: &str,
        restart_current: bool,
    ) -> bool {
        let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        if inner.closed {
            return false;
        }
        if let Some(current) = inner.current.as_ref()
            && current.job.session.id == session_id
        {
            if !restart_current && !current.job.token.is_preempted() {
                return false;
            }
            current.job.token.preempt();
            let current_job = current.job.clone();
            let job = inner.take_queued_session(session_id).unwrap_or(current_job);
            let job = inner.refresh_job(job, HlsFillPriority::Foreground);
            inner.foreground.push_back(job);
            drop(inner);
            self.notify.notify_one();
            return true;
        }

        let Some(job) = inner.take_queued_session(session_id) else {
            return false;
        };
        if let Some(current) = inner.current.as_ref() {
            current.job.token.preempt();
        }
        let job = inner.refresh_job(job, HlsFillPriority::Foreground);
        inner.foreground.push_back(job);
        drop(inner);
        self.notify.notify_one();
        true
    }

    #[cfg(test)]
    pub(crate) fn worker_started_for_tests(&self) -> bool {
        self.inner
            .lock()
            .expect("HLS fill scheduler lock poisoned")
            .worker_started
    }

    #[cfg(test)]
    pub(crate) fn queued_session_count_for_tests(&self, session_id: &str) -> usize {
        let inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        inner
            .foreground
            .iter()
            .chain(inner.demoted.iter())
            .filter(|job| job.session.id == session_id)
            .count()
    }

    fn enqueue(
        &self,
        task_id: String,
        session: HlsPlaybackSession,
        failure_mode: HlsCacheFinalizationFailureMode,
        priority: HlsFillPriority,
        preempt_current: bool,
    ) -> bool {
        let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        if inner.closed {
            return false;
        }
        if preempt_current
            && inner
                .current
                .as_ref()
                .is_some_and(|current| current.job.session.id != session.id)
            && let Some(current) = inner.current.as_ref()
        {
            current.job.token.preempt();
        }
        let job = inner.create_job(task_id, session, failure_mode, priority);
        match priority {
            HlsFillPriority::Foreground => inner.foreground.push_back(job),
            HlsFillPriority::Demoted => inner.demoted.push(job),
        }

        let should_start_worker = !inner.worker_started;
        inner.worker_started = true;
        should_start_worker
    }

    fn mark_worker_stopped(&self) {
        self.inner
            .lock()
            .expect("HLS fill scheduler lock poisoned")
            .worker_started = false;
        self.current_finished.notify_waiters();
        self.notify.notify_one();
    }
}

impl HlsFillSchedulerInner {
    fn create_job(
        &mut self,
        task_id: String,
        session: HlsPlaybackSession,
        failure_mode: HlsCacheFinalizationFailureMode,
        priority: HlsFillPriority,
    ) -> HlsFillJob {
        self.next_sequence = self.next_sequence.saturating_add(1);
        HlsFillJob {
            task_id,
            session,
            failure_mode,
            priority,
            token: HlsFillPreemptionToken::default(),
            sequence: self.next_sequence,
        }
    }

    fn refresh_job(&mut self, mut job: HlsFillJob, priority: HlsFillPriority) -> HlsFillJob {
        self.next_sequence = self.next_sequence.saturating_add(1);
        job.priority = priority;
        job.token = HlsFillPreemptionToken::default();
        job.sequence = self.next_sequence;
        job
    }

    fn take_queued_session(&mut self, session_id: &str) -> Option<HlsFillJob> {
        if let Some(index) = self
            .foreground
            .iter()
            .position(|job| job.session.id == session_id)
        {
            return self.foreground.remove(index);
        }

        let index = self
            .demoted
            .iter()
            .position(|job| job.session.id == session_id)?;
        Some(self.demoted.remove(index))
    }

    fn has_queued_session(&self, session_id: &str) -> bool {
        self.foreground
            .iter()
            .chain(self.demoted.iter())
            .any(|job| job.session.id == session_id)
    }
}

impl HlsFillPreemptionToken {
    pub(crate) fn preempt(&self) {
        self.preempted.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_preempted(&self) -> bool {
        self.preempted.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        bbdown_adapter::{
            BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest,
            BilibiliMediaRequestKind,
        },
        grpc_services::HlsCacheFinalizationFailureMode,
        hls::{HlsMediaResource, HlsPlaybackSession, HlsVariant},
    };

    use super::*;

    #[tokio::test]
    async fn foreground_enqueue_preempts_current_fill() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "old-task".to_owned(),
            sample_session("old-task"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let old_job = scheduler.next_job().await;
        assert_eq!("old-task", old_job.task_id);
        assert!(!old_job.token.is_preempted());

        assert!(!scheduler.enqueue_foreground(
            "new-task".to_owned(),
            sample_session("new-task"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));

        assert!(old_job.token.is_preempted());
        scheduler.finish_current(&old_job, false);
        let new_job = scheduler.next_job().await;
        assert_eq!("new-task", new_job.task_id);
    }

    #[tokio::test]
    async fn foreground_queue_prefers_newest_playback_fill() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "active-task".to_owned(),
            sample_session("active-task"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let active_job = scheduler.next_job().await;

        assert!(!scheduler.enqueue_foreground(
            "older-queued-task".to_owned(),
            sample_session("older-queued-task"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(!scheduler.enqueue_foreground(
            "newest-queued-task".to_owned(),
            sample_session("newest-queued-task"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));

        assert!(active_job.token.is_preempted());
        scheduler.finish_current(&active_job, false);
        let newest_job = scheduler.next_job().await;
        assert_eq!("newest-queued-task", newest_job.task_id);
        scheduler.finish_current(&newest_job, false);
        let older_job = scheduler.next_job().await;
        assert_eq!("older-queued-task", older_job.task_id);
    }

    #[tokio::test]
    async fn demoted_fill_queue_is_lifo() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_demoted(
            "older-task".to_owned(),
            sample_session("older-task"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(!scheduler.enqueue_demoted(
            "newer-task".to_owned(),
            sample_session("newer-task"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(scheduler.owns_session("older-task"));
        assert!(scheduler.owns_session("newer-task"));

        let first = scheduler.next_job().await;
        assert_eq!("newer-task", first.task_id);
        assert!(scheduler.owns_session("newer-task"));
        scheduler.finish_current(&first, false);
        assert!(!scheduler.owns_session("newer-task"));
        let second = scheduler.next_job().await;
        assert_eq!("older-task", second.task_id);
    }

    #[tokio::test]
    async fn cancelling_task_waits_for_current_and_removes_queued_jobs_without_requeue() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "task-a".to_owned(),
            sample_session("session-a-current"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let current = scheduler.next_job().await;
        assert!(!scheduler.enqueue_demoted(
            "task-a".to_owned(),
            sample_session("session-a-queued"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(!scheduler.enqueue_demoted(
            "task-b".to_owned(),
            sample_session("session-b"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));

        let cancelling_scheduler = scheduler.clone();
        let cancellation = tokio::spawn(async move {
            cancelling_scheduler.cancel_task_and_wait("task-a").await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !current.token.is_cancelled()
                || scheduler.queued_session_count_for_tests("session-a-queued") != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task cancellation should reach the active and queued fills");

        assert!(current.token.is_cancelled());
        assert_eq!(
            0,
            scheduler.queued_session_count_for_tests("session-a-queued")
        );
        assert!(!cancellation.is_finished());
        scheduler.finish_current(&current, true);
        tokio::time::timeout(std::time::Duration::from_secs(1), cancellation)
            .await
            .expect("task cancellation should finish after the active fill exits")
            .expect("task cancellation waiter should not panic");
        let remaining = scheduler.next_job().await;
        assert_eq!("task-b", remaining.task_id);
    }

    #[tokio::test]
    async fn cancelling_sessions_preserves_unselected_siblings_for_the_same_task() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "shared-task".to_owned(),
            sample_session("session-delete"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let current = scheduler.next_job().await;
        assert!(!scheduler.enqueue_demoted(
            "shared-task".to_owned(),
            sample_session("session-keep"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let session_ids = HashSet::from(["session-delete".to_owned()]);

        let cancelling_scheduler = scheduler.clone();
        let cancellation_session_ids = session_ids.clone();
        let cancellation = tokio::spawn(async move {
            cancelling_scheduler
                .cancel_sessions_and_wait(&cancellation_session_ids)
                .await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !current.token.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session cancellation should reach the active fill");

        assert_eq!(1, scheduler.queued_session_count_for_tests("session-keep"));
        assert!(!cancellation.is_finished());
        scheduler.finish_current(&current, true);
        tokio::time::timeout(std::time::Duration::from_secs(1), cancellation)
            .await
            .expect("session cancellation should finish after the active fill exits")
            .expect("session cancellation waiter should not panic");
        let remaining = scheduler.next_job().await;
        assert_eq!("session-keep", remaining.session.id);
    }

    #[tokio::test]
    async fn shutdown_wakes_idle_worker_and_waits_for_worker_guard() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "task-a".to_owned(),
            sample_session("session-a"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let current = scheduler.next_job().await;
        scheduler.finish_current(&current, false);

        let worker_scheduler = scheduler.clone();
        let worker = tokio::spawn(async move {
            let _guard = worker_scheduler.worker_guard();
            assert!(worker_scheduler.next_job_until_shutdown().await.is_none());
        });

        scheduler.shutdown_and_wait_for_worker().await;
        worker.await.expect("idle HLS fill worker should exit");
        assert!(!scheduler.worker_started_for_tests());
        assert!(scheduler.is_idle());
        assert!(!scheduler.enqueue_demoted(
            "task-b".to_owned(),
            sample_session("session-b"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(scheduler.is_idle());
    }

    #[tokio::test]
    async fn promotion_preempts_current_and_prefers_active_session() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "task-a".to_owned(),
            sample_session("session-a"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let active_job = scheduler.next_job().await;
        assert_eq!("session-a", active_job.session.id);

        assert!(!scheduler.enqueue_demoted(
            "task-b".to_owned(),
            sample_session("session-b"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(scheduler.promote_session_to_foreground("session-b", false));

        assert!(active_job.token.is_preempted());
        scheduler.finish_current(&active_job, false);
        let promoted = scheduler.next_job().await;
        assert_eq!("session-b", promoted.session.id);
        assert_eq!(HlsFillPriority::Foreground, promoted.priority);
    }

    #[tokio::test]
    async fn promotion_reschedules_preempted_current_session() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "task-a".to_owned(),
            sample_session("session-a"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let active_job = scheduler.next_job().await;
        assert_eq!("session-a", active_job.session.id);

        assert!(!scheduler.enqueue_foreground(
            "task-b".to_owned(),
            sample_session("session-b"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(active_job.token.is_preempted());

        assert!(scheduler.promote_session_to_foreground("session-a", false));
        assert_eq!(1, scheduler.queued_session_count_for_tests("session-a"));
        assert_eq!(1, scheduler.queued_session_count_for_tests("session-b"));

        scheduler.finish_current(&active_job, false);
        let promoted = scheduler.next_job().await;
        assert_eq!("session-a", promoted.session.id);
        assert_eq!(HlsFillPriority::Foreground, promoted.priority);
        scheduler.finish_current(&promoted, false);
        let displaced = scheduler.next_job().await;
        assert_eq!("session-b", displaced.session.id);
    }

    #[tokio::test]
    async fn promotion_keeps_current_session_running_for_heartbeat() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "task-a".to_owned(),
            sample_session("session-a"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let active_job = scheduler.next_job().await;
        assert_eq!("session-a", active_job.session.id);
        assert!(!active_job.token.is_preempted());

        assert!(!scheduler.promote_session_to_foreground("session-a", false));

        assert!(!active_job.token.is_preempted());
        assert_eq!(0, scheduler.queued_session_count_for_tests("session-a"));
    }

    #[tokio::test]
    async fn promotion_restarts_current_session_for_new_playback_position() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "task-a".to_owned(),
            sample_session("session-a"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let active_job = scheduler.next_job().await;
        assert_eq!("session-a", active_job.session.id);
        assert!(!active_job.token.is_preempted());

        assert!(scheduler.promote_session_to_foreground("session-a", true));

        assert!(active_job.token.is_preempted());
        assert_eq!(1, scheduler.queued_session_count_for_tests("session-a"));

        scheduler.finish_current(&active_job, false);
        let restarted = scheduler.next_job().await;
        assert_eq!("session-a", restarted.session.id);
        assert_eq!(HlsFillPriority::Foreground, restarted.priority);
        assert!(!restarted.token.is_preempted());
    }

    #[tokio::test]
    async fn requeue_preempted_skips_when_session_was_repromoted() {
        let scheduler = HlsFillScheduler::default();
        assert!(scheduler.enqueue_foreground(
            "task-a".to_owned(),
            sample_session("session-a"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        let active_job = scheduler.next_job().await;

        assert!(!scheduler.enqueue_foreground(
            "task-b".to_owned(),
            sample_session("session-b"),
            HlsCacheFinalizationFailureMode::KeepPlayable,
        ));
        assert!(scheduler.promote_session_to_foreground("session-a", false));

        scheduler.finish_current(&active_job, true);
        assert_eq!(1, scheduler.queued_session_count_for_tests("session-a"));
    }

    fn sample_session(id: &str) -> HlsPlaybackSession {
        HlsPlaybackSession {
            id: id.to_owned(),
            title: "Episode".to_owned(),
            variant: HlsVariant {
                id: "h264".to_owned(),
                bandwidth: 1_000_000,
                codecs: vec!["avc1.640028".to_owned()],
                width: Some(1920),
                height: Some(1080),
                duration_seconds: 60,
                video: HlsMediaResource {
                    id: "video.m4s".to_owned(),
                    request: BilibiliMediaRequest {
                        kind: BilibiliMediaRequestKind::Video,
                        stream_id: None,
                        url: "https://example.test/video.m4s".to_owned(),
                        backup_urls: Vec::new(),
                        headers: vec![BilibiliHttpHeader {
                            name: "referer".to_owned(),
                            value: "https://www.bilibili.com".to_owned(),
                        }],
                        mime_type: Some("video/mp4".to_owned()),
                        codecs: Some("avc1.640028".to_owned()),
                        bandwidth: Some(1_000_000),
                        width: Some(1920),
                        height: Some(1080),
                        frame_rate: Some("60".to_owned()),
                        size: Some(1024),
                        duration_seconds: Some(60),
                        cache_key: BilibiliMediaCacheKey {
                            content_id: id.to_owned(),
                            media_kind: BilibiliMediaRequestKind::Video,
                            stream_id: None,
                            codecs: Some("avc1.640028".to_owned()),
                            source_hash: "source-hash".to_owned(),
                        },
                    },
                },
                audio: None,
            },
            alternate_variants: Vec::new(),
            advertise_alternate_variants: true,
            abr: Default::default(),
            variants: Vec::new(),
            transcoding: Default::default(),
            effective_policy: crate::playback_policy::PlaybackPolicy::default(),
        }
    }
}
