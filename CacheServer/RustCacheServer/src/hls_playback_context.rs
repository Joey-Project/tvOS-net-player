use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::suspend_clock::{SuspendClock, SuspendTimestamp, SystemSuspendClock};
use uuid::{Uuid, Variant, Version};

const CONTEXT_VALIDITY: Duration = Duration::from_secs(5);
const MAX_CONTEXTS: usize = 256;
const MAX_CONTEXTS_PER_SESSION: usize = 8;
const MAX_PLAYBACK_RATE: f64 = 16.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PlaybackTimingSample {
    pub(crate) playhead_seconds: f64,
    pub(crate) continuous_buffered_end_seconds: f64,
    pub(crate) playback_rate: f64,
}

impl PlaybackTimingSample {
    fn is_valid(self) -> bool {
        self.playhead_seconds.is_finite()
            && self.playhead_seconds >= 0.0
            && self.continuous_buffered_end_seconds.is_finite()
            && self.continuous_buffered_end_seconds >= self.playhead_seconds
            && self.playback_rate.is_finite()
            && self.playback_rate > 0.0
            && self.playback_rate <= MAX_PLAYBACK_RATE
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlaybackContextBinding {
    pub(crate) session_id: String,
    pub(crate) session_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlaybackContextTicket {
    pub(crate) context_id: String,
    pub(crate) challenge: String,
    pub(crate) session_id: String,
    pub(crate) session_generation: u64,
    pub(crate) next_sequence: u64,
    pub(crate) validity_millis: u64,
    pub(crate) http_reader_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlaybackContextSnapshot {
    pub(crate) context_id: String,
    pub(crate) session_id: String,
    pub(crate) session_generation: u64,
    pub(crate) sample: PlaybackTimingSample,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlaybackContextStoreError {
    InvalidIdentifier,
    InvalidBinding,
    InvalidSequence,
    InvalidSample,
    AtCapacity,
    UnknownContext,
    StaleTicket,
    SequenceExhausted,
    DeadlineOverflow,
}

#[derive(Clone)]
pub(crate) struct HlsPlaybackContextStore {
    contexts: Arc<Mutex<HashMap<String, PlaybackContext>>>,
    clock: Arc<dyn SuspendClock>,
    clock_state: Arc<Mutex<ClockState>>,
}

#[derive(Default)]
struct ClockState {
    last: Option<SuspendTimestamp>,
    failed: bool,
}

#[derive(Clone)]
struct PlaybackContext {
    session_id: String,
    session_generation: u64,
    http_reader_id: String,
    challenge: String,
    next_sequence: u64,
    challenge_deadline: SuspendTimestamp,
    sample: Option<ExpiringSample>,
}

#[derive(Clone)]
struct ExpiringSample {
    value: PlaybackTimingSample,
    expires_at: SuspendTimestamp,
}

impl Default for HlsPlaybackContextStore {
    fn default() -> Self {
        Self {
            contexts: Arc::new(Mutex::new(HashMap::new())),
            clock: Arc::new(SystemSuspendClock),
            clock_state: Arc::new(Mutex::new(ClockState::default())),
        }
    }
}

impl HlsPlaybackContextStore {
    pub(crate) fn open(
        &self,
        session_id: String,
        session_generation: u64,
    ) -> Result<PlaybackContextTicket, PlaybackContextStoreError> {
        self.open_with_reader_id(session_id, session_generation, String::new())
    }

    pub(crate) fn open_with_reader_id(
        &self,
        session_id: String,
        session_generation: u64,
        http_reader_id: String,
    ) -> Result<PlaybackContextTicket, PlaybackContextStoreError> {
        self.open_with_clock(session_id, session_generation, http_reader_id, || {
            self.clock.now()
        })
    }

    #[cfg(test)]
    pub(crate) fn open_at(
        &self,
        session_id: String,
        session_generation: u64,
        now: SuspendTimestamp,
    ) -> Result<PlaybackContextTicket, PlaybackContextStoreError> {
        self.open_with_clock(session_id, session_generation, String::new(), || Some(now))
    }

    #[cfg(test)]
    pub(crate) fn open_with_reader_id_at(
        &self,
        session_id: String,
        session_generation: u64,
        http_reader_id: String,
        now: SuspendTimestamp,
    ) -> Result<PlaybackContextTicket, PlaybackContextStoreError> {
        self.open_with_clock(session_id, session_generation, http_reader_id, || Some(now))
    }

    fn open_with_clock(
        &self,
        session_id: String,
        session_generation: u64,
        http_reader_id: String,
        clock: impl FnOnce() -> Option<SuspendTimestamp>,
    ) -> Result<PlaybackContextTicket, PlaybackContextStoreError> {
        if !http_reader_id.is_empty() && !is_canonical_uuid_v4(&http_reader_id) {
            return Err(PlaybackContextStoreError::InvalidIdentifier);
        }
        if session_generation == 0 {
            return Err(PlaybackContextStoreError::InvalidBinding);
        }
        let mut contexts = self
            .contexts
            .lock()
            .expect("HLS playback context store lock poisoned");
        let Some(now) = self.checked_now(clock) else {
            contexts.clear();
            return Err(PlaybackContextStoreError::DeadlineOverflow);
        };
        let Some(deadline) = now.checked_add(CONTEXT_VALIDITY) else {
            self.fail_clock();
            contexts.clear();
            return Err(PlaybackContextStoreError::DeadlineOverflow);
        };
        prune_expired(&mut contexts, now);
        if contexts.len() >= MAX_CONTEXTS
            || contexts
                .values()
                .filter(|context| context.session_id == session_id)
                .count()
                >= MAX_CONTEXTS_PER_SESSION
        {
            return Err(PlaybackContextStoreError::AtCapacity);
        }

        let context_id = unique_context_id(&contexts);
        let context = PlaybackContext {
            session_id,
            session_generation,
            http_reader_id,
            challenge: new_challenge(None),
            next_sequence: 1,
            challenge_deadline: deadline,
            sample: None,
        };
        let ticket = ticket(&context_id, &context);
        contexts.insert(context_id, context);
        Ok(ticket)
    }

    pub(crate) fn binding_for_update(&self, context_id: &str) -> Option<PlaybackContextBinding> {
        self.binding_for_update_with_clock(context_id, || self.clock.now())
    }

    #[cfg(test)]
    pub(crate) fn binding_for_update_at(
        &self,
        context_id: &str,
        now: SuspendTimestamp,
    ) -> Option<PlaybackContextBinding> {
        self.binding_for_update_with_clock(context_id, || Some(now))
    }

    fn binding_for_update_with_clock(
        &self,
        context_id: &str,
        clock: impl FnOnce() -> Option<SuspendTimestamp>,
    ) -> Option<PlaybackContextBinding> {
        let mut contexts = self
            .contexts
            .lock()
            .expect("HLS playback context store lock poisoned");
        let Some(now) = self.checked_now(clock) else {
            contexts.clear();
            return None;
        };
        prune_expired(&mut contexts, now);
        contexts
            .get(context_id)
            .map(|context| PlaybackContextBinding {
                session_id: context.session_id.clone(),
                session_generation: context.session_generation,
            })
    }

    pub(crate) fn update(
        &self,
        context_id: &str,
        challenge: &str,
        sequence: u64,
        sample: PlaybackTimingSample,
    ) -> Result<PlaybackContextTicket, PlaybackContextStoreError> {
        self.update_with_clock(context_id, challenge, sequence, sample, || self.clock.now())
    }

    #[cfg(test)]
    pub(crate) fn update_at(
        &self,
        context_id: &str,
        challenge: &str,
        sequence: u64,
        sample: PlaybackTimingSample,
        now: SuspendTimestamp,
    ) -> Result<PlaybackContextTicket, PlaybackContextStoreError> {
        self.update_with_clock(context_id, challenge, sequence, sample, || Some(now))
    }

    fn update_with_clock(
        &self,
        context_id: &str,
        challenge: &str,
        sequence: u64,
        sample: PlaybackTimingSample,
        clock: impl FnOnce() -> Option<SuspendTimestamp>,
    ) -> Result<PlaybackContextTicket, PlaybackContextStoreError> {
        if !is_uuid_syntax(context_id) || !is_uuid_syntax(challenge) {
            return Err(PlaybackContextStoreError::InvalidIdentifier);
        }
        if sequence == 0 {
            return Err(PlaybackContextStoreError::InvalidSequence);
        }
        if !sample.is_valid() {
            return Err(PlaybackContextStoreError::InvalidSample);
        }
        let mut contexts = self
            .contexts
            .lock()
            .expect("HLS playback context store lock poisoned");
        let Some(now) = self.checked_now(clock) else {
            contexts.clear();
            return Err(PlaybackContextStoreError::DeadlineOverflow);
        };
        let Some(next_deadline) = now.checked_add(CONTEXT_VALIDITY) else {
            self.fail_clock();
            contexts.clear();
            return Err(PlaybackContextStoreError::DeadlineOverflow);
        };
        prune_expired(&mut contexts, now);
        let context = contexts
            .get(context_id)
            .ok_or(PlaybackContextStoreError::UnknownContext)?;
        if context.challenge != challenge || context.next_sequence != sequence {
            return Err(PlaybackContextStoreError::StaleTicket);
        }
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(PlaybackContextStoreError::SequenceExhausted)?;
        let consumed_deadline = context.challenge_deadline;
        let next_challenge = new_challenge(Some(challenge));
        let context = contexts
            .get_mut(context_id)
            .expect("context was checked while holding its store lock");
        context.sample = Some(ExpiringSample {
            value: sample,
            expires_at: consumed_deadline,
        });
        context.challenge = next_challenge;
        context.next_sequence = next_sequence;
        context.challenge_deadline = next_deadline;
        Ok(ticket(context_id, context))
    }

    pub(crate) fn close(&self, context_id: &str) -> bool {
        self.contexts
            .lock()
            .expect("HLS playback context store lock poisoned")
            .remove(context_id)
            .is_some()
    }

    pub(crate) fn remove_session(&self, session_id: &str) {
        self.contexts
            .lock()
            .expect("HLS playback context store lock poisoned")
            .retain(|_, context| context.session_id != session_id);
    }

    pub(crate) fn remove_context_if_binding(
        &self,
        context_id: &str,
        session_id: &str,
        session_generation: u64,
    ) {
        let mut contexts = self
            .contexts
            .lock()
            .expect("HLS playback context store lock poisoned");
        if contexts.get(context_id).is_some_and(|context| {
            context.session_id == session_id && context.session_generation == session_generation
        }) {
            contexts.remove(context_id);
        }
    }

    pub(crate) fn sole_live_snapshot(&self) -> Option<PlaybackContextSnapshot> {
        self.sole_live_snapshot_with_clock(|| self.clock.now())
    }

    pub(crate) fn sole_live_snapshot_for_reader(
        &self,
        http_reader_id: &str,
    ) -> Option<PlaybackContextSnapshot> {
        self.sole_live_snapshot_for_reader_with_clock(http_reader_id, || self.clock.now())
    }

    #[cfg(test)]
    pub(crate) fn sole_live_snapshot_at(
        &self,
        now: SuspendTimestamp,
    ) -> Option<PlaybackContextSnapshot> {
        self.sole_live_snapshot_with_clock(|| Some(now))
    }

    #[cfg(test)]
    pub(crate) fn sole_live_snapshot_for_reader_at(
        &self,
        http_reader_id: &str,
        now: SuspendTimestamp,
    ) -> Option<PlaybackContextSnapshot> {
        self.sole_live_snapshot_for_reader_with_clock(http_reader_id, || Some(now))
    }

    fn sole_live_snapshot_with_clock(
        &self,
        clock: impl FnOnce() -> Option<SuspendTimestamp>,
    ) -> Option<PlaybackContextSnapshot> {
        let mut contexts = self
            .contexts
            .lock()
            .expect("HLS playback context store lock poisoned");
        let Some(now) = self.checked_now(clock) else {
            contexts.clear();
            return None;
        };
        prune_expired(&mut contexts, now);
        if contexts.len() != 1 {
            return None;
        }
        let (context_id, context) = contexts.iter().next()?;
        let sample = context.sample.as_ref()?;
        if !is_deadline_live(sample.expires_at, now) {
            return None;
        }
        Some(PlaybackContextSnapshot {
            context_id: context_id.clone(),
            session_id: context.session_id.clone(),
            session_generation: context.session_generation,
            sample: sample.value,
        })
    }

    fn sole_live_snapshot_for_reader_with_clock(
        &self,
        http_reader_id: &str,
        clock: impl FnOnce() -> Option<SuspendTimestamp>,
    ) -> Option<PlaybackContextSnapshot> {
        if !is_canonical_uuid_v4(http_reader_id) {
            return None;
        }
        let mut contexts = self
            .contexts
            .lock()
            .expect("HLS playback context store lock poisoned");
        let Some(now) = self.checked_now(clock) else {
            contexts.clear();
            return None;
        };
        prune_expired(&mut contexts, now);
        let mut matching = contexts
            .iter()
            .filter(|(_, context)| context.http_reader_id == http_reader_id);
        let (context_id, context) = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        let sample = context.sample.as_ref()?;
        if !is_deadline_live(sample.expires_at, now) {
            return None;
        }
        Some(PlaybackContextSnapshot {
            context_id: context_id.clone(),
            session_id: context.session_id.clone(),
            session_generation: context.session_generation,
            sample: sample.value,
        })
    }

    fn checked_now(
        &self,
        clock: impl FnOnce() -> Option<SuspendTimestamp>,
    ) -> Option<SuspendTimestamp> {
        let mut state = self
            .clock_state
            .lock()
            .expect("HLS playback context clock lock poisoned");
        if state.failed {
            return None;
        }
        let Some(now) = clock() else {
            state.failed = true;
            return None;
        };
        if state.last.is_some_and(|last| now < last) {
            state.failed = true;
            return None;
        }
        state.last = Some(now);
        Some(now)
    }

    fn fail_clock(&self) {
        self.clock_state
            .lock()
            .expect("HLS playback context clock lock poisoned")
            .failed = true;
    }
}

pub(crate) fn is_uuid_syntax(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

pub(crate) fn is_canonical_uuid_v4(value: &str) -> bool {
    let Ok(uuid) = Uuid::parse_str(value) else {
        return false;
    };
    uuid.to_string() == value
        && uuid.get_version() == Some(Version::Random)
        && uuid.get_variant() == Variant::RFC4122
}

fn ticket(context_id: &str, context: &PlaybackContext) -> PlaybackContextTicket {
    PlaybackContextTicket {
        context_id: context_id.to_owned(),
        challenge: context.challenge.clone(),
        session_id: context.session_id.clone(),
        session_generation: context.session_generation,
        next_sequence: context.next_sequence,
        validity_millis: CONTEXT_VALIDITY.as_millis() as u64,
        http_reader_id: context.http_reader_id.clone(),
    }
}

fn new_challenge(previous: Option<&str>) -> String {
    loop {
        let challenge = Uuid::new_v4().to_string();
        if previous != Some(challenge.as_str()) {
            return challenge;
        }
    }
}

fn unique_context_id(contexts: &HashMap<String, PlaybackContext>) -> String {
    loop {
        let context_id = Uuid::new_v4().to_string();
        if !contexts.contains_key(&context_id) {
            return context_id;
        }
    }
}

fn is_deadline_live(deadline: SuspendTimestamp, now: SuspendTimestamp) -> bool {
    deadline
        .checked_elapsed_since(now)
        .is_some_and(|remaining| !remaining.is_zero())
}

fn prune_expired(contexts: &mut HashMap<String, PlaybackContext>, now: SuspendTimestamp) {
    contexts.retain(|_, context| is_deadline_live(context.challenge_deadline, now));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suspend_clock::SuspendTimestamp;

    fn timestamp(nanoseconds: u128) -> SuspendTimestamp {
        SuspendTimestamp::from_nanos(nanoseconds)
    }

    fn after(timestamp: SuspendTimestamp, duration: Duration) -> SuspendTimestamp {
        timestamp
            .checked_add(duration)
            .expect("test timestamp should not overflow")
    }

    fn sample(playhead_seconds: f64) -> PlaybackTimingSample {
        PlaybackTimingSample {
            playhead_seconds,
            continuous_buffered_end_seconds: playhead_seconds + 2.0,
            playback_rate: 1.0,
        }
    }

    fn open(
        store: &HlsPlaybackContextStore,
        session_id: &str,
        now: SuspendTimestamp,
    ) -> PlaybackContextTicket {
        store
            .open_at(session_id.to_owned(), 1, now)
            .expect("context should open")
    }

    #[test]
    fn tickets_rotate_and_sample_keeps_consumed_challenge_deadline() {
        let store = HlsPlaybackContextStore::default();
        let now = timestamp(1_000_000_000);
        let first = open(&store, "session-a", now);
        assert_eq!(1, first.next_sequence);
        assert_eq!(5_000, first.validity_millis);

        let accepted_at = after(now, Duration::from_millis(4_999));
        let next = store
            .update_at(
                &first.context_id,
                &first.challenge,
                first.next_sequence,
                sample(4.0),
                accepted_at,
            )
            .expect("live challenge should be consumed");
        assert_ne!(first.challenge, next.challenge);
        assert_eq!(2, next.next_sequence);
        assert_eq!(
            Some(sample(4.0)),
            store
                .sole_live_snapshot_at(accepted_at)
                .map(|snapshot| snapshot.sample)
        );
        assert!(
            store
                .sole_live_snapshot_at(after(now, Duration::from_secs(5)))
                .is_none()
        );
        assert!(
            store
                .binding_for_update_at(&next.context_id, after(now, Duration::from_secs(5)))
                .is_some()
        );
    }

    #[test]
    fn continuous_clock_advance_expires_sample_and_prunes_context() {
        let store = HlsPlaybackContextStore::default();
        let opened_at = timestamp(10_000_000_000);
        let first = open(&store, "session-a", opened_at);
        let sampled_at = after(opened_at, Duration::from_secs(1));
        let next = store
            .update_at(
                &first.context_id,
                &first.challenge,
                first.next_sequence,
                sample(4.0),
                sampled_at,
            )
            .expect("live challenge should be consumed after the test clock advances");
        assert!(
            store
                .sole_live_snapshot_at(after(opened_at, Duration::from_secs(4)))
                .is_some()
        );

        let sleep_advanced_clock = after(opened_at, CONTEXT_VALIDITY);
        assert!(store.sole_live_snapshot_at(sleep_advanced_clock).is_none());
        assert!(
            store
                .binding_for_update_at(&next.context_id, sleep_advanced_clock)
                .is_some()
        );
        assert_eq!(
            Err(PlaybackContextStoreError::UnknownContext),
            store.update_at(
                &next.context_id,
                &next.challenge,
                next.next_sequence,
                sample(5.0),
                after(opened_at, Duration::from_secs(6)),
            )
        );
        assert!(
            store
                .contexts
                .lock()
                .expect("store lock should be available")
                .is_empty()
        );
    }

    #[test]
    fn unavailable_or_backwards_clock_withholds_and_retires_contexts() {
        let store = HlsPlaybackContextStore::default();
        let first = open(&store, "session-a", timestamp(20_000_000_000));
        assert_eq!(
            None,
            store.binding_for_update_with_clock(&first.context_id, || None)
        );
        assert!(
            store
                .sole_live_snapshot_at(timestamp(20_000_000_001))
                .is_none()
        );
        assert!(
            store
                .contexts
                .lock()
                .expect("store lock should be available")
                .is_empty()
        );

        let store = HlsPlaybackContextStore::default();
        let first = open(&store, "session-b", timestamp(30_000_000_000));
        assert_eq!(
            None,
            store.binding_for_update_at(&first.context_id, timestamp(29_999_999_999))
        );
        assert_eq!(
            Err(PlaybackContextStoreError::DeadlineOverflow),
            store.open_at("session-c".to_owned(), 1, timestamp(30_000_000_001))
        );
    }

    #[test]
    fn context_deadline_overflow_is_rejected() {
        let store = HlsPlaybackContextStore::default();
        let existing = open(&store, "session-a", timestamp(0));
        assert_eq!(
            Err(PlaybackContextStoreError::DeadlineOverflow),
            store.open_at("session-b".to_owned(), 1, timestamp(u128::MAX - 1))
        );
        assert!(
            store
                .contexts
                .lock()
                .expect("store lock should be available")
                .is_empty()
        );
        assert!(
            store
                .binding_for_update_at(&existing.context_id, timestamp(u128::MAX))
                .is_none()
        );
    }

    #[test]
    fn queued_update_reads_clock_after_lock_and_rejects_expired_challenge() {
        let store = HlsPlaybackContextStore::default();
        let now = timestamp(2_000_000_000);
        let first = open(&store, "session-a", now);
        let clock = Arc::new(Mutex::new(after(
            now,
            CONTEXT_VALIDITY - Duration::from_millis(1),
        )));
        let guard = store
            .contexts
            .lock()
            .expect("store lock should be available");
        let worker_store = store.clone();
        let worker_clock = Arc::clone(&clock);
        let context_id = first.context_id.clone();
        let challenge = first.challenge.clone();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_sender
                .send(())
                .expect("test coordinator should remain available");
            let clock_store = worker_store.clone();
            let result = worker_store.update_with_clock(
                &context_id,
                &challenge,
                1,
                sample(1.0),
                move || {
                    assert!(clock_store.contexts.try_lock().is_err());
                    Some(
                        *worker_clock
                            .lock()
                            .expect("test clock lock should be available"),
                    )
                },
            );
            result_sender
                .send(result)
                .expect("test coordinator should remain available");
        });
        started_receiver
            .recv()
            .expect("update worker should start before the clock advances");
        *clock.lock().expect("test clock lock should be available") = after(now, CONTEXT_VALIDITY);
        drop(guard);

        assert_eq!(
            Err(PlaybackContextStoreError::UnknownContext),
            result_receiver
                .recv()
                .expect("queued update should finish after the store lock is released")
        );
        worker.join().expect("update worker should finish");
        assert!(
            store
                .binding_for_update_at(&first.context_id, after(now, CONTEXT_VALIDITY))
                .is_none()
        );
    }

    #[test]
    fn replay_and_wrong_sequence_leave_current_challenge_and_sample_unchanged() {
        let store = HlsPlaybackContextStore::default();
        let now = timestamp(3_000_000_000);
        let first = open(&store, "session-a", now);
        let second = store
            .update_at(
                &first.context_id,
                &first.challenge,
                1,
                sample(1.0),
                after(now, Duration::from_secs(1)),
            )
            .expect("first sample should be accepted");
        let before = store
            .contexts
            .lock()
            .expect("store lock should be available")
            .get(&first.context_id)
            .cloned()
            .expect("context should exist");

        assert_eq!(
            Err(PlaybackContextStoreError::StaleTicket),
            store.update_at(
                &first.context_id,
                &first.challenge,
                1,
                sample(10.0),
                after(now, Duration::from_secs(2)),
            )
        );
        assert_eq!(
            Err(PlaybackContextStoreError::StaleTicket),
            store.update_at(
                &first.context_id,
                &second.challenge,
                9,
                sample(10.0),
                after(now, Duration::from_secs(2)),
            )
        );
        let after = store
            .contexts
            .lock()
            .expect("store lock should be available")
            .get(&first.context_id)
            .cloned()
            .expect("context should remain unchanged");
        assert_eq!(before.challenge, after.challenge);
        assert_eq!(before.next_sequence, after.next_sequence);
        assert_eq!(before.challenge_deadline, after.challenge_deadline);
        assert_eq!(
            before.sample.map(|value| value.value),
            after.sample.map(|value| value.value)
        );
    }

    #[test]
    fn rejects_invalid_samples_and_uuid_fields() {
        let store = HlsPlaybackContextStore::default();
        let now = timestamp(4_000_000_000);
        let first = open(&store, "session-a", now);
        for invalid in [
            PlaybackTimingSample {
                playhead_seconds: f64::NAN,
                ..sample(0.0)
            },
            PlaybackTimingSample {
                continuous_buffered_end_seconds: f64::INFINITY,
                ..sample(0.0)
            },
            PlaybackTimingSample {
                continuous_buffered_end_seconds: -1.0,
                ..sample(0.0)
            },
            PlaybackTimingSample {
                playback_rate: f64::NAN,
                ..sample(0.0)
            },
            PlaybackTimingSample {
                playback_rate: 0.0,
                ..sample(0.0)
            },
            PlaybackTimingSample {
                playback_rate: 16.01,
                ..sample(0.0)
            },
        ] {
            assert_eq!(
                Err(PlaybackContextStoreError::InvalidSample),
                store.update_at(
                    &first.context_id,
                    &first.challenge,
                    1,
                    invalid,
                    after(now, Duration::from_secs(1)),
                )
            );
        }
        assert_eq!(
            Err(PlaybackContextStoreError::InvalidIdentifier),
            store.update_at("not-a-uuid", &first.challenge, 1, sample(0.0), now)
        );
        assert_eq!(
            Err(PlaybackContextStoreError::InvalidIdentifier),
            store.update_at(&first.context_id, "not-a-uuid", 1, sample(0.0), now)
        );
        assert_eq!(
            Err(PlaybackContextStoreError::InvalidSequence),
            store.update_at(&first.context_id, &first.challenge, 0, sample(0.0), now)
        );
    }

    #[test]
    fn enforces_global_and_per_session_caps_and_prunes_before_admission() {
        let store = HlsPlaybackContextStore::default();
        let now = timestamp(5_000_000_000);
        for session_index in 0..(MAX_CONTEXTS / MAX_CONTEXTS_PER_SESSION) {
            for _ in 0..MAX_CONTEXTS_PER_SESSION {
                store
                    .open_at(format!("session-{session_index}"), 1, now)
                    .expect("admission below both caps should succeed");
            }
        }
        assert_eq!(
            Err(PlaybackContextStoreError::AtCapacity),
            store.open_at("session-0".to_owned(), 1, now)
        );
        assert_eq!(
            Err(PlaybackContextStoreError::AtCapacity),
            store.open_at("session-overflow".to_owned(), 1, now)
        );
        let after_expiry = after(now, CONTEXT_VALIDITY);
        let ticket = store
            .open_at("session-after-expiry".to_owned(), 1, after_expiry)
            .expect("expired entries should be pruned before admission");
        assert!(
            store
                .binding_for_update_at(&ticket.context_id, after_expiry)
                .is_some()
        );
    }

    #[test]
    fn sole_snapshot_requires_one_owner_and_a_fresh_sample() {
        let store = HlsPlaybackContextStore::default();
        let now = timestamp(6_000_000_000);
        let first = open(&store, "session-a", now);
        assert!(store.sole_live_snapshot_at(now).is_none());
        store
            .update_at(&first.context_id, &first.challenge, 1, sample(1.0), now)
            .expect("sample should be accepted");
        assert!(store.sole_live_snapshot_at(now).is_some());

        let second = open(&store, "session-b", now);
        assert!(store.sole_live_snapshot_at(now).is_none());
        store.remove_session("session-b");
        assert!(store.sole_live_snapshot_at(now).is_some());

        let _third = open(&store, "session-a", now);
        assert!(store.sole_live_snapshot_at(now).is_none());
        assert!(
            store
                .sole_live_snapshot_at(after(now, CONTEXT_VALIDITY))
                .is_none()
        );
        assert!(
            store
                .binding_for_update_at(&second.context_id, now)
                .is_none()
        );
    }

    #[test]
    fn close_and_session_removal_only_remove_their_exact_contexts() {
        let store = HlsPlaybackContextStore::default();
        let now = timestamp(7_000_000_000);
        let first = open(&store, "session-a", now);
        let second = open(&store, "session-b", now);

        assert!(store.close(&first.context_id));
        assert!(!store.close(&first.context_id));
        assert!(
            store
                .binding_for_update_at(&second.context_id, now)
                .is_some()
        );
        store.remove_session("session-a");
        assert!(
            store
                .binding_for_update_at(&second.context_id, now)
                .is_some()
        );
        store.remove_session("session-b");
        assert!(
            store
                .binding_for_update_at(&second.context_id, now)
                .is_none()
        );
    }

    #[test]
    fn reader_ids_must_be_canonical_lowercase_rfc4122_v4() {
        let valid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        assert!(is_canonical_uuid_v4(valid));
        for invalid in [
            "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
            "aaaaaaaa-aaaa-3aaa-8aaa-aaaaaaaaaaaa",
            "aaaaaaaa-aaaa-4aaa-0aaa-aaaaaaaaaaaa",
            "aaaaaaaaaaaa4aaa8aaaaaaaaaaaaaaa",
            "{aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa}",
            "00000000-0000-0000-0000-000000000000",
        ] {
            assert!(!is_canonical_uuid_v4(invalid), "accepted {invalid}");
        }
        assert_eq!(
            Err(PlaybackContextStoreError::InvalidIdentifier),
            HlsPlaybackContextStore::default().open_with_reader_id_at(
                "session-a".to_owned(),
                1,
                "aaaaaaaa-aaaa-3aaa-8aaa-aaaaaaaaaaaa".to_owned(),
                timestamp(1),
            )
        );
    }

    #[test]
    fn reader_snapshots_are_independent_and_duplicate_live_contexts_are_ambiguous() {
        let store = HlsPlaybackContextStore::default();
        let now = timestamp(8_000_000_000);
        let reader_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let reader_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let first = store
            .open_with_reader_id_at("session-a".to_owned(), 2, reader_a.to_owned(), now)
            .expect("reader A context should open");
        let second = store
            .open_with_reader_id_at("session-b".to_owned(), 7, reader_b.to_owned(), now)
            .expect("reader B context should open");
        assert_eq!(reader_a, first.http_reader_id);
        assert_eq!(reader_b, second.http_reader_id);
        store
            .update_at(&first.context_id, &first.challenge, 1, sample(1.0), now)
            .expect("reader A sample should be accepted");
        store
            .update_at(&second.context_id, &second.challenge, 1, sample(4.0), now)
            .expect("reader B sample should be accepted");
        assert_eq!(
            Some(sample(1.0)),
            store
                .sole_live_snapshot_for_reader_at(reader_a, now)
                .map(|snapshot| snapshot.sample)
        );
        assert_eq!(
            Some(sample(4.0)),
            store
                .sole_live_snapshot_for_reader_at(reader_b, now)
                .map(|snapshot| snapshot.sample)
        );

        let duplicate = store
            .open_with_reader_id_at("session-c".to_owned(), 1, reader_a.to_owned(), now)
            .expect("second live reader A context should open");
        assert!(
            store
                .sole_live_snapshot_for_reader_at(reader_a, now)
                .is_none()
        );
        store.close(&duplicate.context_id);
        assert_eq!(
            Some(sample(1.0)),
            store
                .sole_live_snapshot_for_reader_at(reader_a, now)
                .map(|snapshot| snapshot.sample)
        );
        store.remove_session("session-a");
        assert!(
            store
                .sole_live_snapshot_for_reader_at(reader_a, now)
                .is_none()
        );
        assert!(
            store
                .sole_live_snapshot_for_reader_at(reader_b, now)
                .is_some()
        );
        assert!(
            store
                .sole_live_snapshot_for_reader_at(reader_b, after(now, CONTEXT_VALIDITY))
                .is_none()
        );
    }
}
