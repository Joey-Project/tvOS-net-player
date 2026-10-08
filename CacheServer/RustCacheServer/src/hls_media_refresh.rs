use std::{
    collections::HashMap,
    future::Future,
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{
    bbdown_adapter::BilibiliMediaRequest,
    hls::{HlsMediaResource, HlsPlaybackSession, HlsVariant},
    hls_cache::{HlsCacheError, HlsCacheFillControl},
    hls_range_cache::HlsRangeError,
    task_registry::BilibiliTaskCancellation,
};

pub(crate) const HLS_MEDIA_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const HLS_MEDIA_REFRESH_COOLDOWN: Duration = Duration::from_secs(60);
pub(crate) const HLS_MEDIA_REFRESH_MAX_SESSIONS: usize = 4_096;
pub(crate) const HLS_MEDIA_REFRESH_MAX_RESOURCE_COOLDOWNS: usize = 64;
pub(crate) const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) enum HlsMediaRefreshError {
    Cancelled,
    Preempted,
    Unavailable,
    IdentityMismatch,
    Persistence(io::Error),
}

impl std::fmt::Debug for HlsMediaRefreshError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "Cancelled",
            Self::Preempted => "Preempted",
            Self::Unavailable => "Unavailable",
            Self::IdentityMismatch => "IdentityMismatch",
            Self::Persistence(_) => "Persistence",
        })
    }
}

impl std::fmt::Display for HlsMediaRefreshError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "media refresh was cancelled",
            Self::Preempted => "media refresh was preempted",
            Self::Unavailable => "media refresh is unavailable",
            Self::IdentityMismatch => "media refresh identity did not match",
            Self::Persistence(_) => "refreshed media requests could not be persisted",
        })
    }
}

impl std::error::Error for HlsMediaRefreshError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persistence(error) => Some(error),
            _ => None,
        }
    }
}

impl HlsMediaRefreshError {
    pub(crate) fn into_range_error(self, unavailable: HlsRangeError) -> HlsRangeError {
        match self {
            Self::Cancelled => HlsRangeError::Cancelled,
            Self::Preempted => HlsRangeError::Preempted,
            Self::IdentityMismatch => HlsRangeError::IdentityChanged,
            Self::Persistence(error) => HlsRangeError::Io(error),
            Self::Unavailable => unavailable,
        }
    }

    pub(crate) fn into_cache_error(self, unavailable: HlsCacheError) -> HlsCacheError {
        match self {
            Self::Cancelled => HlsCacheError::Cancelled,
            Self::Preempted => HlsCacheError::Preempted,
            Self::Persistence(error) => HlsCacheError::Io(error),
            Self::IdentityMismatch => HlsCacheError::Range(HlsRangeError::IdentityChanged),
            Self::Unavailable => unavailable,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct HlsMediaRefreshCoordinator {
    entries: Arc<Mutex<HashMap<String, Arc<HlsMediaRefreshEntry>>>>,
}

pub(crate) struct HlsMediaRefreshEntry {
    lock: Arc<AsyncMutex<()>>,
    last_attempts: Mutex<HashMap<Option<String>, Instant>>,
}

impl HlsMediaRefreshEntry {
    pub(crate) fn begin_attempt(&self, resource_id: Option<&str>, now: Instant) -> bool {
        let key = resource_id.map(str::to_owned);
        let mut last_attempts = self
            .last_attempts
            .lock()
            .expect("HLS media refresh cooldown lock poisoned");
        if last_attempts
            .get(&key)
            .is_some_and(|last| now.saturating_duration_since(*last) < HLS_MEDIA_REFRESH_COOLDOWN)
        {
            return false;
        }
        last_attempts
            .retain(|_, last| now.saturating_duration_since(*last) < HLS_MEDIA_REFRESH_COOLDOWN);
        if !last_attempts.contains_key(&key)
            && last_attempts.len() >= HLS_MEDIA_REFRESH_MAX_RESOURCE_COOLDOWNS
        {
            return false;
        }
        last_attempts.insert(key, now);
        true
    }

    pub(crate) fn clear_attempt_if_current(&self, resource_id: Option<&str>, attempt: Instant) {
        let key = resource_id.map(str::to_owned);
        let mut last_attempts = self
            .last_attempts
            .lock()
            .expect("HLS media refresh cooldown lock poisoned");
        if last_attempts.get(&key) == Some(&attempt) {
            last_attempts.remove(&key);
        }
    }
}

impl HlsMediaRefreshCoordinator {
    pub(crate) async fn lock_session<F>(
        &self,
        session_id: &str,
        control: &F,
    ) -> Result<(Arc<HlsMediaRefreshEntry>, OwnedMutexGuard<()>), HlsMediaRefreshError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        check_control(control)?;
        let entry = self.entry(session_id)?;
        let lock = Arc::clone(&entry.lock).lock_owned();
        tokio::pin!(lock);
        let deadline = tokio::time::Instant::now() + HLS_MEDIA_REFRESH_TIMEOUT;
        let guard = loop {
            check_control(control)?;
            tokio::select! {
                guard = &mut lock => break guard,
                () = tokio::time::sleep_until(deadline) => return Err(HlsMediaRefreshError::Unavailable),
                () = tokio::time::sleep(CONTROL_POLL_INTERVAL) => {}
            }
        };
        Ok((entry, guard))
    }

    pub(crate) fn remove_session(&self, session_id: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(session_id);
        }
    }

    fn entry(&self, session_id: &str) -> Result<Arc<HlsMediaRefreshEntry>, HlsMediaRefreshError> {
        let mut entries = self
            .entries
            .lock()
            .expect("HLS media refresh map lock poisoned");
        if let Some(entry) = entries.get(session_id) {
            return Ok(Arc::clone(entry));
        }
        if entries.len() >= HLS_MEDIA_REFRESH_MAX_SESSIONS {
            let now = Instant::now();
            entries.retain(|_, entry| {
                if Arc::strong_count(entry) != 1 {
                    return true;
                }
                let idle = entry
                    .last_attempts
                    .lock()
                    .expect("HLS media refresh cooldown lock poisoned")
                    .values()
                    .all(|last| now.saturating_duration_since(*last) >= HLS_MEDIA_REFRESH_COOLDOWN);
                !idle || entry.lock.try_lock().is_err()
            });
        }
        if entries.len() >= HLS_MEDIA_REFRESH_MAX_SESSIONS {
            return Err(HlsMediaRefreshError::Unavailable);
        }
        let entry = Arc::new(HlsMediaRefreshEntry {
            lock: Arc::new(AsyncMutex::new(())),
            last_attempts: Mutex::new(HashMap::new()),
        });
        entries.insert(session_id.to_owned(), Arc::clone(&entry));
        Ok(entry)
    }
}

pub(crate) fn check_control<F>(control: &F) -> Result<(), HlsMediaRefreshError>
where
    F: Fn() -> HlsCacheFillControl + Send + Sync,
{
    match control() {
        HlsCacheFillControl::Continue => Ok(()),
        HlsCacheFillControl::Cancel => Err(HlsMediaRefreshError::Cancelled),
        HlsCacheFillControl::Preempt => Err(HlsMediaRefreshError::Preempted),
    }
}

pub(crate) async fn await_plan_with_control<T, E, F, Fut>(
    future: Fut,
    cancellation: &BilibiliTaskCancellation,
    control: &F,
    timeout: Duration,
) -> Result<T, HlsMediaRefreshError>
where
    Fut: Future<Output = Result<T, E>> + Send,
    F: Fn() -> HlsCacheFillControl + Send + Sync,
{
    tokio::pin!(future);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Err(error) = check_control(control) {
            cancellation.request_cancel();
            return Err(error);
        }
        tokio::select! {
            result = &mut future => return result.map_err(|_| HlsMediaRefreshError::Unavailable),
            () = tokio::time::sleep_until(deadline) => {
                cancellation.request_cancel();
                return Err(HlsMediaRefreshError::Unavailable);
            }
            () = tokio::time::sleep(CONTROL_POLL_INTERVAL) => {}
        }
    }
}

pub(crate) fn same_hls_media_refresh_binding(
    expected: &HlsPlaybackSession,
    current: &HlsPlaybackSession,
) -> bool {
    let mut expected = expected.clone();
    let mut current = current.clone();
    clear_request_urls_and_headers(&mut expected.variant);
    clear_request_urls_and_headers(&mut current.variant);
    for variant in &mut expected.alternate_variants {
        clear_request_urls_and_headers(variant);
    }
    for variant in &mut current.alternate_variants {
        clear_request_urls_and_headers(variant);
    }
    expected == current
}

pub(crate) fn media_resource_request_changed(
    expected: &HlsMediaResource,
    current: &HlsPlaybackSession,
) -> bool {
    current.media_resource(&expected.id).is_some_and(|current| {
        expected.request.url != current.request.url
            || expected.request.backup_urls != current.request.backup_urls
            || expected.request.headers != current.request.headers
    })
}

pub(crate) fn failed_media_request_was_refreshed(
    expected: &HlsPlaybackSession,
    current: &HlsPlaybackSession,
    failed_resource: Option<&HlsMediaResource>,
) -> bool {
    if let Some(resource) = failed_resource {
        return media_resource_request_changed(resource, current);
    }
    if !same_hls_media_refresh_binding(expected, current) {
        return false;
    }
    let expected_requests = session_resource_requests(expected);
    let current_requests = session_resource_requests(current);
    !expected_requests.is_empty()
        && expected_requests.len() == current_requests.len()
        && expected_requests.iter().zip(&current_requests).all(
            |((expected_id, expected), (current_id, current))| {
                expected_id == current_id && request_urls_or_headers_differ(expected, current)
            },
        )
}

pub(crate) fn preserve_concurrent_media_request_updates(
    expected: &HlsPlaybackSession,
    current: &HlsPlaybackSession,
    replacement: &mut HlsPlaybackSession,
    refreshed_resource_id: Option<&str>,
) {
    let expected_requests = session_resource_requests(expected);
    let current_requests = session_resource_requests(current);
    let mut replacement_requests = session_resource_requests_mut(replacement);
    for (((expected_id, expected), (current_id, current)), (replacement_id, replacement)) in
        expected_requests
            .into_iter()
            .zip(current_requests)
            .zip(replacement_requests.iter_mut())
    {
        if expected_id == current_id
            && current_id == replacement_id
            && refreshed_resource_id != Some(current_id)
            && request_urls_or_headers_differ(expected, current)
        {
            replacement.url.clone_from(&current.url);
            replacement.backup_urls.clone_from(&current.backup_urls);
            replacement.headers.clone_from(&current.headers);
        }
    }
}

fn request_urls_or_headers_differ(
    expected: &BilibiliMediaRequest,
    current: &BilibiliMediaRequest,
) -> bool {
    expected.url != current.url
        || expected.backup_urls != current.backup_urls
        || expected.headers != current.headers
}

fn session_resource_requests(session: &HlsPlaybackSession) -> Vec<(&str, &BilibiliMediaRequest)> {
    std::iter::once(&session.variant)
        .chain(session.alternate_variants.iter())
        .flat_map(|variant| {
            std::iter::once((variant.video.id.as_str(), &variant.video.request)).chain(
                variant
                    .audio
                    .iter()
                    .map(|audio| (audio.id.as_str(), &audio.request)),
            )
        })
        .collect()
}

fn session_resource_requests_mut(
    session: &mut HlsPlaybackSession,
) -> Vec<(String, &mut BilibiliMediaRequest)> {
    let mut requests = Vec::new();
    for variant in
        std::iter::once(&mut session.variant).chain(session.alternate_variants.iter_mut())
    {
        requests.push((variant.video.id.clone(), &mut variant.video.request));
        if let Some(audio) = &mut variant.audio {
            requests.push((audio.id.clone(), &mut audio.request));
        }
    }
    requests
}

fn clear_request_urls_and_headers(variant: &mut HlsVariant) {
    clear_request(&mut variant.video.request);
    if let Some(audio) = &mut variant.audio {
        clear_request(&mut audio.request);
    }
}

fn clear_request(request: &mut BilibiliMediaRequest) {
    request.url.clear();
    request.backup_urls.clear();
    request.headers.clear();
}

pub(crate) fn is_expired_range_error(error: &HlsRangeError) -> bool {
    match error {
        HlsRangeError::UpstreamStatus(status) => is_expired_status(*status),
        _ => false,
    }
}

pub(crate) fn is_expired_cache_error(error: &HlsCacheError) -> bool {
    match error {
        HlsCacheError::UpstreamStatus(status) => is_expired_status(*status),
        HlsCacheError::Range(range_error) => is_expired_range_error(range_error),
        _ => false,
    }
}

fn is_expired_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[test]
    fn refresh_binding_ignores_only_signed_request_fields() {
        let original = crate::tests::sample_hls_session("session");
        let mut refreshed = original.clone();
        refreshed.variant.video.request.url = "https://cdn.example/renewed".to_owned();
        refreshed.variant.video.request.backup_urls =
            vec!["https://backup.example/renewed".to_owned()];
        refreshed.variant.video.request.headers = vec![crate::bbdown_adapter::BilibiliHttpHeader {
            name: "accept-language".to_owned(),
            value: "en".to_owned(),
        }];
        assert!(same_hls_media_refresh_binding(&original, &refreshed));
        assert!(media_resource_request_changed(
            &original.variant.video,
            &refreshed
        ));
        assert!(!media_resource_request_changed(
            &original.variant.video,
            &original
        ));

        refreshed.variant.video.request.cache_key.source_hash = "different".to_owned();
        assert!(!same_hls_media_refresh_binding(&original, &refreshed));
    }

    #[test]
    fn refresh_detection_is_scoped_to_the_failed_media_resource() {
        let mut original = crate::tests::sample_hls_session("session");
        let mut audio = original.variant.video.clone();
        audio.id = "audio.m4s".to_owned();
        audio.request.kind = crate::bbdown_adapter::BilibiliMediaRequestKind::Audio;
        audio.request.url = "https://cdn.example.test/audio-old.m4s".to_owned();
        audio.request.cache_key.media_kind = crate::bbdown_adapter::BilibiliMediaRequestKind::Audio;
        audio.request.cache_key.source_hash = "audio-source".to_owned();
        original.variant.audio = Some(audio);
        let mut alternate = original.variant.clone();
        alternate.id = "alternate".to_owned();
        alternate.video.id = "alternate-video.m4s".to_owned();
        alternate.video.request.url = "https://cdn.example.test/alternate-old.m4s".to_owned();
        alternate.video.request.cache_key.source_hash = "alternate-source".to_owned();
        original.alternate_variants.push(alternate);

        let mut current = original.clone();
        current.variant.video.request.url = "https://cdn.example.test/video-fresh.m4s".to_owned();
        current.alternate_variants[0].video.request.url =
            "https://cdn.example.test/alternate-fresh.m4s".to_owned();
        assert!(media_resource_request_changed(
            &original.variant.video,
            &current
        ));
        assert!(!media_resource_request_changed(
            original.variant.audio.as_ref().unwrap(),
            &current
        ));
        assert!(media_resource_request_changed(
            &original.alternate_variants[0].video,
            &current
        ));

        current.variant.audio.as_mut().unwrap().request.headers[0].value =
            "https://headers.example.test/audio".to_owned();
        assert!(media_resource_request_changed(
            original.variant.audio.as_ref().unwrap(),
            &current
        ));
        assert!(!failed_media_request_was_refreshed(
            &original, &current, None
        ));
        current.variant.audio.as_mut().unwrap().request.url =
            "https://cdn.example.test/audio-fresh.m4s".to_owned();
        current.alternate_variants[0]
            .audio
            .as_mut()
            .unwrap()
            .request
            .url = "https://cdn.example.test/alternate-audio-fresh.m4s".to_owned();
        assert!(failed_media_request_was_refreshed(
            &original, &current, None
        ));
        let single = crate::tests::sample_hls_session("single");
        let mut single_refreshed = single.clone();
        single_refreshed.variant.video.request.url =
            "https://cdn.example.test/video-fresh.m4s".to_owned();
        assert!(failed_media_request_was_refreshed(
            &single,
            &single_refreshed,
            None
        ));
    }

    #[test]
    fn refresh_merge_preserves_other_resources_updated_since_failure() {
        let mut expected = crate::tests::sample_hls_session("session");
        let mut audio = expected.variant.video.clone();
        audio.id = "audio.m4s".to_owned();
        audio.request.kind = crate::bbdown_adapter::BilibiliMediaRequestKind::Audio;
        audio.request.url = "https://cdn.example.test/audio-old.m4s".to_owned();
        audio.request.cache_key.media_kind = crate::bbdown_adapter::BilibiliMediaRequestKind::Audio;
        audio.request.cache_key.source_hash = "audio-source".to_owned();
        expected.variant.audio = Some(audio);
        let mut alternate = expected.variant.clone();
        alternate.id = "alternate".to_owned();
        alternate.video.id = "alternate-video.m4s".to_owned();
        alternate.video.request.url = "https://cdn.example.test/alternate-old.m4s".to_owned();
        alternate.video.request.cache_key.source_hash = "alternate-source".to_owned();
        expected.alternate_variants.push(alternate);

        let mut current = expected.clone();
        current.variant.video.request.url = "https://cdn.example.test/video-current.m4s".to_owned();
        current.alternate_variants[0].video.request.url =
            "https://cdn.example.test/alternate-current.m4s".to_owned();
        let mut replacement = current.clone();
        replacement.variant.video.request.url =
            "https://cdn.example.test/video-stale.m4s".to_owned();
        replacement.variant.audio.as_mut().unwrap().request.url =
            "https://cdn.example.test/audio-refreshed.m4s".to_owned();
        replacement.alternate_variants[0].video.request.url =
            "https://cdn.example.test/alternate-stale.m4s".to_owned();

        preserve_concurrent_media_request_updates(
            &expected,
            &current,
            &mut replacement,
            Some("audio.m4s"),
        );

        assert_eq!(
            "https://cdn.example.test/video-current.m4s",
            replacement.variant.video.request.url
        );
        assert_eq!(
            "https://cdn.example.test/audio-refreshed.m4s",
            replacement.variant.audio.as_ref().unwrap().request.url
        );
        assert_eq!(
            "https://cdn.example.test/alternate-current.m4s",
            replacement.alternate_variants[0].video.request.url
        );
    }

    #[test]
    fn expiration_predicates_accept_only_expiration_statuses() {
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::GONE,
        ] {
            assert!(is_expired_range_error(&HlsRangeError::UpstreamStatus(
                status
            )));
            assert!(is_expired_cache_error(&HlsCacheError::UpstreamStatus(
                status
            )));
            assert!(is_expired_cache_error(&HlsCacheError::Range(
                HlsRangeError::UpstreamStatus(status)
            )));
        }
        for status in [StatusCode::BAD_GATEWAY, StatusCode::SERVICE_UNAVAILABLE] {
            assert!(!is_expired_range_error(&HlsRangeError::UpstreamStatus(
                status
            )));
            assert!(!is_expired_cache_error(&HlsCacheError::UpstreamStatus(
                status
            )));
        }
        assert!(!is_expired_range_error(&HlsRangeError::RangeUnsupported));
    }

    #[tokio::test]
    async fn same_session_waiter_observes_failed_attempt_cooldown() {
        let coordinator = HlsMediaRefreshCoordinator::default();
        let (entry, first_guard) = coordinator
            .lock_session("session", &|| HlsCacheFillControl::Continue)
            .await
            .expect("first caller should acquire session lock");
        let now = Instant::now();
        assert!(entry.begin_attempt(Some("video.m4s"), now));

        let waiter_coordinator = coordinator.clone();
        let waiter = tokio::spawn(async move {
            waiter_coordinator
                .lock_session("session", &|| HlsCacheFillControl::Continue)
                .await
        });
        drop(first_guard);
        let (waiter_entry, _waiter_guard) = waiter
            .await
            .expect("waiter should finish")
            .expect("waiter should acquire session lock");
        assert!(!waiter_entry.begin_attempt(Some("video.m4s"), now + Duration::from_secs(1)));
        assert!(waiter_entry.begin_attempt(Some("audio.m4s"), now + Duration::from_secs(1)));
    }

    #[test]
    fn resource_cooldown_is_bounded_and_cleared_only_for_its_attempt() {
        let entry = HlsMediaRefreshEntry {
            lock: Arc::new(AsyncMutex::new(())),
            last_attempts: Mutex::new(HashMap::new()),
        };
        let start = Instant::now();
        for index in 0..HLS_MEDIA_REFRESH_MAX_RESOURCE_COOLDOWNS {
            assert!(entry.begin_attempt(Some(&format!("resource-{index}")), start));
        }
        assert_eq!(
            HLS_MEDIA_REFRESH_MAX_RESOURCE_COOLDOWNS,
            entry.last_attempts.lock().unwrap().len()
        );
        assert!(!entry.begin_attempt(Some("new-resource"), start + Duration::from_secs(1)));
        for index in 0..HLS_MEDIA_REFRESH_MAX_RESOURCE_COOLDOWNS {
            assert!(!entry.begin_attempt(
                Some(&format!("resource-{index}")),
                start + Duration::from_secs(1)
            ));
        }
        assert_eq!(
            HLS_MEDIA_REFRESH_MAX_RESOURCE_COOLDOWNS,
            entry.last_attempts.lock().unwrap().len()
        );
        let after_cooldown = start + HLS_MEDIA_REFRESH_COOLDOWN;
        assert!(entry.begin_attempt(Some("new-resource"), after_cooldown));
        assert_eq!(1, entry.last_attempts.lock().unwrap().len());
        entry.clear_attempt_if_current(
            Some("new-resource"),
            after_cooldown + Duration::from_secs(1),
        );
        assert!(!entry.begin_attempt(
            Some("new-resource"),
            after_cooldown + Duration::from_secs(2)
        ));
        entry.clear_attempt_if_current(Some("new-resource"), after_cooldown);
        assert!(entry.begin_attempt(
            Some("new-resource"),
            after_cooldown + Duration::from_secs(2)
        ));
    }

    #[tokio::test]
    async fn waiting_refresh_observes_preemption() {
        let coordinator = HlsMediaRefreshCoordinator::default();
        let (_entry, first_guard) = coordinator
            .lock_session("session", &|| HlsCacheFillControl::Continue)
            .await
            .expect("first caller should acquire session lock");
        let preempted = Arc::new(AtomicBool::new(false));
        let waiter_coordinator = coordinator.clone();
        let waiter_preempted = Arc::clone(&preempted);
        let waiter = tokio::spawn(async move {
            waiter_coordinator
                .lock_session("session", &|| {
                    if waiter_preempted.load(Ordering::Relaxed) {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                })
                .await
        });
        preempted.store(true, Ordering::Relaxed);
        assert!(matches!(
            waiter.await.expect("waiter should finish"),
            Err(HlsMediaRefreshError::Preempted)
        ));
        drop(first_guard);
    }

    #[tokio::test]
    async fn waiting_refresh_observes_cancellation() {
        let coordinator = HlsMediaRefreshCoordinator::default();
        let (_entry, first_guard) = coordinator
            .lock_session("session", &|| HlsCacheFillControl::Continue)
            .await
            .expect("first caller should acquire session lock");
        let cancelled = Arc::new(AtomicBool::new(false));
        let waiter_coordinator = coordinator.clone();
        let waiter_cancelled = Arc::clone(&cancelled);
        let waiter = tokio::spawn(async move {
            waiter_coordinator
                .lock_session("session", &|| {
                    if waiter_cancelled.load(Ordering::Relaxed) {
                        HlsCacheFillControl::Cancel
                    } else {
                        HlsCacheFillControl::Continue
                    }
                })
                .await
        });
        cancelled.store(true, Ordering::Relaxed);
        assert!(matches!(
            waiter.await.expect("waiter should finish"),
            Err(HlsMediaRefreshError::Cancelled)
        ));
        drop(first_guard);
    }

    #[tokio::test]
    async fn planning_timeout_drops_future_and_cancels_planner_token() {
        let cancellation = BilibiliTaskCancellation::default();
        let result = await_plan_with_control(
            std::future::pending::<Result<(), ()>>(),
            &cancellation,
            &|| HlsCacheFillControl::Continue,
            Duration::from_millis(5),
        )
        .await;
        assert!(matches!(result, Err(HlsMediaRefreshError::Unavailable)));
        assert!(cancellation.is_cancel_requested());
    }

    #[tokio::test]
    async fn planning_control_change_cancels_planner_token() {
        let cancellation = BilibiliTaskCancellation::default();
        let control_state = Arc::new(AtomicBool::new(false));
        let signal_state = Arc::clone(&control_state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            signal_state.store(true, Ordering::Relaxed);
        });
        let result = await_plan_with_control(
            std::future::pending::<Result<(), ()>>(),
            &cancellation,
            &|| {
                if control_state.load(Ordering::Relaxed) {
                    HlsCacheFillControl::Cancel
                } else {
                    HlsCacheFillControl::Continue
                }
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(HlsMediaRefreshError::Cancelled)));
        assert!(cancellation.is_cancel_requested());
    }

    #[test]
    fn unavailable_error_preserves_original_expiration_error() {
        let range_error = HlsRangeError::UpstreamStatus(StatusCode::UNAUTHORIZED);
        assert!(matches!(
            HlsMediaRefreshError::Unavailable.into_range_error(range_error),
            HlsRangeError::UpstreamStatus(StatusCode::UNAUTHORIZED)
        ));
        let cache_error = HlsCacheError::Range(HlsRangeError::UpstreamStatus(StatusCode::GONE));
        assert!(matches!(
            HlsMediaRefreshError::Unavailable.into_cache_error(cache_error),
            HlsCacheError::Range(HlsRangeError::UpstreamStatus(StatusCode::GONE))
        ));
    }

    #[test]
    fn coordinator_reclaims_idle_entries_at_capacity() {
        let coordinator = HlsMediaRefreshCoordinator::default();
        for index in 0..HLS_MEDIA_REFRESH_MAX_SESSIONS {
            coordinator
                .entry(&format!("session-{index}"))
                .expect("entry should fit within capacity");
        }
        let replacement = coordinator
            .entry("replacement")
            .expect("an idle entry should be reclaimed to admit the replacement");
        drop(replacement);
        assert_eq!(
            coordinator
                .entries
                .lock()
                .expect("entry map should remain available")
                .len(),
            1
        );
    }

    #[test]
    fn coordinator_rejects_overflow_when_entries_are_retained_and_recent() {
        let coordinator = HlsMediaRefreshCoordinator::default();
        let now = Instant::now();
        let mut retained = Vec::with_capacity(HLS_MEDIA_REFRESH_MAX_SESSIONS);
        for index in 0..HLS_MEDIA_REFRESH_MAX_SESSIONS {
            let entry = coordinator
                .entry(&format!("session-{index}"))
                .expect("entry should fit within capacity");
            assert!(entry.begin_attempt(None, now));
            retained.push(entry);
        }
        assert!(matches!(
            coordinator.entry("overflow"),
            Err(HlsMediaRefreshError::Unavailable)
        ));
        assert_eq!(HLS_MEDIA_REFRESH_MAX_SESSIONS, retained.len());
        assert_eq!(
            coordinator
                .entries
                .lock()
                .expect("entry map should remain available")
                .len(),
            HLS_MEDIA_REFRESH_MAX_SESSIONS
        );
    }
}
