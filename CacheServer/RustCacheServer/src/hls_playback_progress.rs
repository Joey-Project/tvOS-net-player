use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use url::Url;

use crate::hls_cache::HlsCacheStore;

const ACTIVE_PLAYBACK_TTL: Duration = Duration::from_secs(45);
const STOPPED_PLAYBACK_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub(crate) struct HlsPlaybackProgressTracker {
    inner: Arc<Mutex<HlsPlaybackProgressState>>,
}

#[derive(Default)]
struct HlsPlaybackProgressState {
    reports_by_session_id: HashMap<String, HlsPlaybackProgressSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlaybackProgressIntent {
    Started,
    Playing,
    Seek,
    Paused,
    Stopped,
}

impl PlaybackProgressIntent {
    pub(crate) fn is_stopped(self) -> bool {
        matches!(self, Self::Stopped)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlaybackProgressReport {
    pub(crate) playback_uri: String,
    pub(crate) library_item_id: String,
    pub(crate) variant_id: String,
    pub(crate) position_seconds: f64,
    pub(crate) duration_seconds: Option<f64>,
    pub(crate) intent: PlaybackProgressIntent,
    pub(crate) reported_at: SystemTime,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlaybackProgressRecordOutcome {
    pub(crate) accepted: bool,
    pub(crate) session_id: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HlsPlaybackProgressSnapshot {
    pub(crate) state: HlsPlaybackActivityState,
    pub(crate) message: String,
    pub(crate) session_id: String,
    pub(crate) library_item_id: String,
    pub(crate) variant_id: String,
    pub(crate) playback_uri: String,
    pub(crate) position_seconds: f64,
    pub(crate) duration_seconds: Option<f64>,
    pub(crate) last_intent: PlaybackProgressIntent,
    pub(crate) updated_at: SystemTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsPlaybackActivityState {
    None,
    Active,
    RecentlyStopped,
}

impl HlsPlaybackProgressTracker {
    pub(crate) fn record(&self, report: PlaybackProgressReport) -> PlaybackProgressRecordOutcome {
        let Some(session_id) = session_id_from_report(&report) else {
            return PlaybackProgressRecordOutcome {
                accepted: false,
                session_id: String::new(),
                message: "Playback URI does not identify an HLS cache session.".to_owned(),
            };
        };

        let snapshot = HlsPlaybackProgressSnapshot {
            state: if report.intent.is_stopped() {
                HlsPlaybackActivityState::RecentlyStopped
            } else {
                HlsPlaybackActivityState::Active
            },
            message: message_for_report(&report),
            session_id: session_id.clone(),
            library_item_id: report.library_item_id.trim().to_owned(),
            variant_id: report.variant_id.trim().to_owned(),
            playback_uri: report.playback_uri.trim().to_owned(),
            position_seconds: report.position_seconds,
            duration_seconds: report.duration_seconds,
            last_intent: report.intent,
            updated_at: report.reported_at,
        };

        let mut state = self
            .inner
            .lock()
            .expect("HLS playback progress lock poisoned");
        state.prune_expired(report.reported_at);
        state
            .reports_by_session_id
            .insert(session_id.clone(), snapshot);

        PlaybackProgressRecordOutcome {
            accepted: true,
            session_id,
            message: "Playback progress recorded.".to_owned(),
        }
    }

    pub(crate) fn snapshot(&self) -> HlsPlaybackProgressSnapshot {
        self.snapshot_at(SystemTime::now())
    }

    pub(crate) fn snapshot_at(&self, now: SystemTime) -> HlsPlaybackProgressSnapshot {
        let mut state = self
            .inner
            .lock()
            .expect("HLS playback progress lock poisoned");
        state.prune_expired(now);
        state
            .reports_by_session_id
            .values()
            .max_by_key(|snapshot| snapshot.updated_at)
            .cloned()
            .unwrap_or_else(empty_snapshot)
    }
}

impl HlsPlaybackProgressState {
    fn prune_expired(&mut self, now: SystemTime) {
        self.reports_by_session_id.retain(|_, snapshot| {
            let ttl = match snapshot.state {
                HlsPlaybackActivityState::Active => ACTIVE_PLAYBACK_TTL,
                HlsPlaybackActivityState::RecentlyStopped => STOPPED_PLAYBACK_TTL,
                HlsPlaybackActivityState::None => Duration::ZERO,
            };
            snapshot
                .updated_at
                .checked_add(ttl)
                .is_some_and(|expires_at| expires_at > now)
        });
    }
}

fn session_id_from_report(report: &PlaybackProgressReport) -> Option<String> {
    let library_item_id = report.library_item_id.trim();
    if let Some(session_id) = HlsCacheStore::session_id_from_library_item_id(library_item_id) {
        return Some(session_id);
    }

    session_id_from_hls_master_uri(report.playback_uri.trim())
}

fn session_id_from_hls_master_uri(uri: &str) -> Option<String> {
    let parsed = Url::parse(uri).ok()?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return None,
    }

    let segments: Vec<_> = parsed.path_segments()?.collect();
    if segments.len() < 3 {
        return None;
    }
    let hls_segment_index = segments.len() - 3;
    if segments[hls_segment_index] != "hls" {
        return None;
    }
    let session_id = segments[hls_segment_index + 1].trim();
    if session_id.is_empty() {
        return None;
    }
    if segments[hls_segment_index + 2] != "master.m3u8" {
        return None;
    }

    Some(session_id.to_owned())
}

fn message_for_report(report: &PlaybackProgressReport) -> String {
    match report.intent {
        PlaybackProgressIntent::Started => "Playback started; keeping nearby HLS cache warm.",
        PlaybackProgressIntent::Playing => "Playback is active; keeping nearby HLS cache warm.",
        PlaybackProgressIntent::Seek => {
            "Playback seek reported; refreshing nearby HLS cache priority."
        }
        PlaybackProgressIntent::Paused => "Playback paused; keeping the HLS cache session recent.",
        PlaybackProgressIntent::Stopped => {
            "Playback stopped; keeping the HLS cache session recent briefly."
        }
    }
    .to_owned()
}

fn empty_snapshot() -> HlsPlaybackProgressSnapshot {
    HlsPlaybackProgressSnapshot {
        state: HlsPlaybackActivityState::None,
        message: "No active HLS playback position reported.".to_owned(),
        session_id: String::new(),
        library_item_id: String::new(),
        variant_id: String::new(),
        playback_uri: String::new(),
        position_seconds: 0.0,
        duration_seconds: None,
        last_intent: PlaybackProgressIntent::Stopped,
        updated_at: SystemTime::UNIX_EPOCH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_hls_master_uri_progress() {
        let tracker = HlsPlaybackProgressTracker::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        let outcome = tracker.record(PlaybackProgressReport {
            playback_uri: "https://cache.example.test/hls/session-1/master.m3u8".to_owned(),
            library_item_id: String::new(),
            variant_id: "h264".to_owned(),
            position_seconds: 42.0,
            duration_seconds: Some(120.0),
            intent: PlaybackProgressIntent::Playing,
            reported_at: now,
        });

        assert!(outcome.accepted);
        assert_eq!("session-1", outcome.session_id);
        let snapshot = tracker.snapshot_at(now);
        assert_eq!(HlsPlaybackActivityState::Active, snapshot.state);
        assert_eq!("session-1", snapshot.session_id);
        assert_eq!("h264", snapshot.variant_id);
        assert_eq!(42.0, snapshot.position_seconds);
    }

    #[test]
    fn records_path_prefixed_hls_master_uri_progress() {
        let tracker = HlsPlaybackProgressTracker::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        let outcome = tracker.record(PlaybackProgressReport {
            playback_uri: "https://cache.example.test/cache/hls/session-1/master.m3u8".to_owned(),
            library_item_id: String::new(),
            variant_id: "h264".to_owned(),
            position_seconds: 42.0,
            duration_seconds: Some(120.0),
            intent: PlaybackProgressIntent::Playing,
            reported_at: now,
        });

        assert!(outcome.accepted);
        assert_eq!("session-1", outcome.session_id);
        assert_eq!("session-1", tracker.snapshot_at(now).session_id);
    }

    #[test]
    fn uses_completed_library_item_id_before_uri() {
        let tracker = HlsPlaybackProgressTracker::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        let outcome = tracker.record(PlaybackProgressReport {
            playback_uri: "https://cache.example.test/hls/runtime-session/master.m3u8".to_owned(),
            library_item_id: "bilibili.hls.completed-session".to_owned(),
            variant_id: String::new(),
            position_seconds: 1.0,
            duration_seconds: None,
            intent: PlaybackProgressIntent::Started,
            reported_at: now,
        });

        assert!(outcome.accepted);
        assert_eq!("completed-session", outcome.session_id);
        assert_eq!(
            "completed-session",
            tracker.snapshot_at(now).session_id.as_str()
        );
    }

    #[test]
    fn reports_non_hls_uri_as_unaccepted() {
        let tracker = HlsPlaybackProgressTracker::default();

        let outcome = tracker.record(PlaybackProgressReport {
            playback_uri: "https://cache.example.test/media/item/original".to_owned(),
            library_item_id: String::new(),
            variant_id: String::new(),
            position_seconds: 0.0,
            duration_seconds: None,
            intent: PlaybackProgressIntent::Started,
            reported_at: SystemTime::UNIX_EPOCH,
        });

        assert!(!outcome.accepted);
        assert!(outcome.session_id.is_empty());
    }

    #[test]
    fn expires_active_and_stopped_reports() {
        let tracker = HlsPlaybackProgressTracker::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        tracker.record(PlaybackProgressReport {
            playback_uri: "https://cache.example.test/hls/session-1/master.m3u8".to_owned(),
            library_item_id: String::new(),
            variant_id: String::new(),
            position_seconds: 0.0,
            duration_seconds: None,
            intent: PlaybackProgressIntent::Playing,
            reported_at: now,
        });

        assert_eq!(
            HlsPlaybackActivityState::None,
            tracker
                .snapshot_at(now + ACTIVE_PLAYBACK_TTL + Duration::from_secs(1))
                .state
        );

        tracker.record(PlaybackProgressReport {
            playback_uri: "https://cache.example.test/hls/session-2/master.m3u8".to_owned(),
            library_item_id: String::new(),
            variant_id: String::new(),
            position_seconds: 0.0,
            duration_seconds: None,
            intent: PlaybackProgressIntent::Stopped,
            reported_at: now,
        });

        assert_eq!(
            HlsPlaybackActivityState::None,
            tracker
                .snapshot_at(now + STOPPED_PLAYBACK_TTL + Duration::from_secs(1))
                .state
        );
    }
}
