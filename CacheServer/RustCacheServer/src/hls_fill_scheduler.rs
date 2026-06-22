use std::{
    collections::VecDeque,
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
}

#[derive(Default)]
struct HlsFillSchedulerInner {
    foreground: VecDeque<HlsFillJob>,
    demoted: Vec<HlsFillJob>,
    current: Option<HlsFillCurrentJob>,
    worker_started: bool,
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
}

struct HlsFillCurrentJob {
    task_id: String,
    token: HlsFillPreemptionToken,
    sequence: u64,
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

    pub(crate) async fn next_job(&self) -> HlsFillJob {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
                if let Some(job) = inner.foreground.pop_back().or_else(|| inner.demoted.pop()) {
                    inner.current = Some(HlsFillCurrentJob {
                        task_id: job.task_id.clone(),
                        token: job.token.clone(),
                        sequence: job.sequence,
                    });
                    return job;
                }
            }
            notified.await;
        }
    }

    pub(crate) fn finish_current(&self, job: &HlsFillJob) {
        let mut inner = self.inner.lock().expect("HLS fill scheduler lock poisoned");
        if inner.current.as_ref().is_some_and(|current| {
            current.sequence == job.sequence && current.task_id == job.task_id
        }) {
            inner.current = None;
        }
    }

    pub(crate) fn requeue_preempted(&self, job: HlsFillJob) {
        let should_start_worker = self.enqueue(
            job.task_id,
            job.session,
            job.failure_mode,
            HlsFillPriority::Demoted,
            false,
        );
        debug_assert!(
            !should_start_worker,
            "HLS fill worker should already be running before preempted jobs are requeued"
        );
        self.notify.notify_one();
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
        if preempt_current
            && inner
                .current
                .as_ref()
                .is_some_and(|current| current.task_id != task_id)
            && let Some(current) = inner.current.as_ref()
        {
            current.token.preempt();
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
}

impl HlsFillPreemptionToken {
    pub(crate) fn preempt(&self) {
        self.preempted.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_preempted(&self) -> bool {
        self.preempted.load(Ordering::SeqCst)
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
        scheduler.finish_current(&old_job);
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
        scheduler.finish_current(&active_job);
        let newest_job = scheduler.next_job().await;
        assert_eq!("newest-queued-task", newest_job.task_id);
        scheduler.finish_current(&newest_job);
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

        let first = scheduler.next_job().await;
        assert_eq!("newer-task", first.task_id);
        scheduler.finish_current(&first);
        let second = scheduler.next_job().await;
        assert_eq!("older-task", second.task_id);
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
            abr: Default::default(),
            variants: Vec::new(),
        }
    }
}
