use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use crate::playback_policy::WeakNetworkPreference;

const RETRYING_WINDOW: Duration = Duration::from_secs(30);
const CACHE_ONLY_WINDOW: Duration = Duration::from_secs(30);
const DEGRADE_DURATION: Duration = Duration::from_secs(120);
const SLOW_RESPONSE_THRESHOLD: Duration = Duration::from_secs(3);
const SLOW_RESPONSE_DEGRADE_COUNT: u32 = 2;
#[cfg(test)]
const TEST_SESSION_GENERATION: u64 = 1;

#[derive(Clone, Default)]
pub(crate) struct HlsNetworkPolicy {
    inner: Arc<Mutex<HlsNetworkPolicyState>>,
}

impl HlsNetworkPolicy {
    pub(crate) fn variant_is_advertisable_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
    ) -> bool {
        self.variant_is_advertisable_at_for_policy(
            weak_network_preference,
            session_id,
            session_generation,
            variant_id,
            SystemTime::now(),
        )
    }

    pub(crate) fn record_upstream_retry_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
    ) {
        self.record_upstream_retry_at_for_policy(
            weak_network_preference,
            session_id,
            session_generation,
            variant_id,
            SystemTime::now(),
        );
    }

    pub(crate) fn record_upstream_success_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
        response_time: Duration,
    ) {
        self.record_upstream_success_at_for_policy(
            weak_network_preference,
            session_id,
            session_generation,
            variant_id,
            response_time,
            SystemTime::now(),
        );
    }

    #[cfg(test)]
    pub(crate) fn record_upstream_failure(&self, session_id: &str, variant_id: &str) {
        self.record_upstream_failure_for_policy(
            WeakNetworkPreference::Adaptive,
            session_id,
            TEST_SESSION_GENERATION,
            variant_id,
        );
    }

    pub(crate) fn record_upstream_failure_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
    ) {
        self.record_upstream_failure_at_for_policy(
            weak_network_preference,
            session_id,
            session_generation,
            variant_id,
            SystemTime::now(),
        );
    }

    pub(crate) fn record_cache_hit_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
    ) {
        self.record_cache_hit_at_for_policy(
            weak_network_preference,
            session_id,
            session_generation,
            SystemTime::now(),
        );
    }

    pub(crate) fn advance_session_generation(&self, session_id: &str, generation: u64) {
        self.advance_session_generation_at(session_id, generation, SystemTime::now());
    }

    pub(crate) fn remove_session_generation(&self, session_id: &str, generation: u64) {
        self.remove_session_generation_at(session_id, generation, SystemTime::now());
    }

    pub(crate) fn snapshot(&self) -> HlsWeakNetworkSnapshot {
        self.snapshot_at(SystemTime::now())
    }

    #[cfg(test)]
    pub(crate) fn session_generation_for_tests(&self, session_id: &str) -> Option<u64> {
        self.inner
            .lock()
            .expect("HLS network policy lock poisoned")
            .sessions
            .get(session_id)
            .map(|session| session.generation)
    }

    #[cfg(test)]
    pub(crate) fn variant_is_advertisable_at(
        &self,
        session_id: &str,
        variant_id: &str,
        now: SystemTime,
    ) -> bool {
        self.variant_is_advertisable_at_for_policy(
            WeakNetworkPreference::Adaptive,
            session_id,
            TEST_SESSION_GENERATION,
            variant_id,
            now,
        )
    }

    pub(crate) fn variant_is_advertisable_at_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
        now: SystemTime,
    ) -> bool {
        if weak_network_preference == WeakNetworkPreference::AvPlayerManaged {
            return true;
        }
        let mut state = self.inner.lock().expect("HLS network policy lock poisoned");
        state.prune_expired(now);
        state
            .sessions
            .get(session_id)
            .filter(|session| session.generation == session_generation)
            .and_then(|session| session.variants.get(variant_id))
            .is_none_or(|variant| !variant.is_degraded(now))
    }

    #[cfg(test)]
    pub(crate) fn record_upstream_retry_at(
        &self,
        session_id: &str,
        variant_id: &str,
        now: SystemTime,
    ) {
        self.record_upstream_retry_at_for_policy(
            WeakNetworkPreference::Adaptive,
            session_id,
            TEST_SESSION_GENERATION,
            variant_id,
            now,
        );
    }

    pub(crate) fn record_upstream_retry_at_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
        now: SystemTime,
    ) {
        if weak_network_preference == WeakNetworkPreference::AvPlayerManaged {
            return;
        }
        let mut state = self.inner.lock().expect("HLS network policy lock poisoned");
        state.prune_expired(now);
        let Some(variant) = state.variant_mut(session_id, session_generation, variant_id) else {
            return;
        };
        variant.retrying_until = Some(now + RETRYING_WINDOW);
        state.last_changed_at = Some(now);
    }

    #[cfg(test)]
    pub(crate) fn record_upstream_success_at(
        &self,
        session_id: &str,
        variant_id: &str,
        response_time: Duration,
        now: SystemTime,
    ) {
        self.record_upstream_success_at_for_policy(
            WeakNetworkPreference::Adaptive,
            session_id,
            TEST_SESSION_GENERATION,
            variant_id,
            response_time,
            now,
        );
    }

    pub(crate) fn record_upstream_success_at_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
        response_time: Duration,
        now: SystemTime,
    ) {
        if weak_network_preference == WeakNetworkPreference::AvPlayerManaged {
            return;
        }
        let mut state = self.inner.lock().expect("HLS network policy lock poisoned");
        state.prune_expired(now);
        let Some(variant) = state.variant_mut(session_id, session_generation, variant_id) else {
            return;
        };
        variant.consecutive_failures = 0;
        if response_time >= SLOW_RESPONSE_THRESHOLD {
            variant.consecutive_slow_responses += 1;
            variant.retrying_until = Some(now + RETRYING_WINDOW);
            if variant.consecutive_slow_responses >= SLOW_RESPONSE_DEGRADE_COUNT {
                set_variant_degraded(variant, weak_network_preference, now);
                variant.unhealthy_reason = Some(HlsWeakNetworkReason::SlowUpstream);
            }
        } else {
            variant.consecutive_slow_responses = 0;
        }
        state.last_changed_at = Some(now);
    }

    #[cfg(test)]
    pub(crate) fn record_upstream_failure_at(
        &self,
        session_id: &str,
        variant_id: &str,
        now: SystemTime,
    ) {
        self.record_upstream_failure_at_for_policy(
            WeakNetworkPreference::Adaptive,
            session_id,
            TEST_SESSION_GENERATION,
            variant_id,
            now,
        );
    }

    pub(crate) fn record_upstream_failure_at_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
        now: SystemTime,
    ) {
        if weak_network_preference == WeakNetworkPreference::AvPlayerManaged {
            return;
        }
        let mut state = self.inner.lock().expect("HLS network policy lock poisoned");
        state.prune_expired(now);
        let Some(variant) = state.variant_mut(session_id, session_generation, variant_id) else {
            return;
        };
        variant.consecutive_failures += 1;
        variant.retrying_until = Some(now + RETRYING_WINDOW);
        set_variant_degraded(variant, weak_network_preference, now);
        variant.unhealthy_reason = Some(HlsWeakNetworkReason::UpstreamFailed);
        state.last_changed_at = Some(now);
    }

    #[cfg(test)]
    pub(crate) fn record_cache_hit_at(&self, session_id: &str, now: SystemTime) {
        self.record_cache_hit_at_for_policy(
            WeakNetworkPreference::Adaptive,
            session_id,
            TEST_SESSION_GENERATION,
            now,
        );
    }

    pub(crate) fn record_cache_hit_at_for_policy(
        &self,
        weak_network_preference: WeakNetworkPreference,
        session_id: &str,
        session_generation: u64,
        now: SystemTime,
    ) {
        if weak_network_preference == WeakNetworkPreference::AvPlayerManaged {
            return;
        }
        let mut state = self.inner.lock().expect("HLS network policy lock poisoned");
        state.prune_expired(now);
        let Some(session) = state.sessions.get_mut(session_id) else {
            return;
        };
        if session.generation != session_generation {
            return;
        }
        if !session.has_degraded_variant(now) {
            return;
        }
        session.cache_only_until = Some(now + CACHE_ONLY_WINDOW);
        state.last_changed_at = Some(now);
    }

    fn advance_session_generation_at(&self, session_id: &str, generation: u64, now: SystemTime) {
        let mut state = self.inner.lock().expect("HLS network policy lock poisoned");
        state.prune_expired(now);
        let should_advance = state
            .sessions
            .get(session_id)
            .is_none_or(|session| session.generation < generation);
        if should_advance {
            let cleared_active_state = state
                .sessions
                .get(session_id)
                .is_some_and(HlsSessionNetworkState::has_active_state);
            state.sessions.insert(
                session_id.to_owned(),
                HlsSessionNetworkState::new(generation),
            );
            if cleared_active_state {
                state.last_changed_at = Some(now);
            }
        }
    }

    fn remove_session_generation_at(&self, session_id: &str, generation: u64, now: SystemTime) {
        let mut state = self.inner.lock().expect("HLS network policy lock poisoned");
        state.prune_expired(now);
        let removed_active_state = state
            .sessions
            .get(session_id)
            .filter(|session| session.generation <= generation)
            .is_some_and(HlsSessionNetworkState::has_active_state);
        let should_remove = state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.generation <= generation);
        if should_remove {
            state.sessions.remove(session_id);
            if removed_active_state {
                state.last_changed_at = Some(now);
            }
        }
    }

    #[cfg(test)]
    fn remove_session_at(&self, session_id: &str, now: SystemTime) {
        self.remove_session_generation_at(session_id, TEST_SESSION_GENERATION, now);
    }

    pub(crate) fn snapshot_at(&self, now: SystemTime) -> HlsWeakNetworkSnapshot {
        let mut state = self.inner.lock().expect("HLS network policy lock poisoned");
        state.prune_expired(now);
        state.snapshot(now)
    }
}

#[derive(Default)]
struct HlsNetworkPolicyState {
    sessions: HashMap<String, HlsSessionNetworkState>,
    last_changed_at: Option<SystemTime>,
}

impl HlsNetworkPolicyState {
    fn variant_mut(
        &mut self,
        session_id: &str,
        session_generation: u64,
        variant_id: &str,
    ) -> Option<&mut HlsVariantNetworkState> {
        let should_advance = self
            .sessions
            .get(session_id)
            .is_none_or(|session| session.generation < session_generation);
        if should_advance {
            self.sessions.insert(
                session_id.to_owned(),
                HlsSessionNetworkState::new(session_generation),
            );
        }
        self.sessions
            .get_mut(session_id)
            .filter(|session| session.generation == session_generation)
            .map(|session| session.variants.entry(variant_id.to_owned()).or_default())
    }

    fn prune_expired(&mut self, now: SystemTime) {
        for session in self.sessions.values_mut() {
            session.variants.retain(|_, variant| {
                variant.retrying_until = variant.retrying_until.filter(|until| *until > now);
                if !variant.hold_degraded
                    && variant.unhealthy_until.is_some_and(|until| until <= now)
                {
                    variant.unhealthy_until = None;
                    variant.unhealthy_reason = None;
                    variant.consecutive_failures = 0;
                    variant.consecutive_slow_responses = 0;
                }
                variant.retrying_until.is_some()
                    || variant.unhealthy_until.is_some()
                    || variant.hold_degraded
            });
            session.cache_only_until = session
                .cache_only_until
                .filter(|until| *until > now && session.has_degraded_variant(now));
        }
    }

    fn snapshot(&self, now: SystemTime) -> HlsWeakNetworkSnapshot {
        let mut retrying_variant_count = 0_usize;
        let mut unhealthy_variant_count = 0_usize;
        let mut degraded_session_count = 0_usize;
        let mut cache_only_session_count = 0_usize;
        let mut saw_upstream_failure = false;

        for session in self.sessions.values() {
            let mut session_degraded = false;
            if session.cache_only_until.is_some_and(|until| until > now) {
                cache_only_session_count += 1;
            }
            for variant in session.variants.values() {
                if variant.retrying_until.is_some_and(|until| until > now) {
                    retrying_variant_count += 1;
                }
                if variant.is_degraded(now) {
                    unhealthy_variant_count += 1;
                    session_degraded = true;
                    saw_upstream_failure |=
                        variant.unhealthy_reason == Some(HlsWeakNetworkReason::UpstreamFailed);
                }
            }
            if session_degraded {
                degraded_session_count += 1;
            }
        }

        let state = if cache_only_session_count > 0 {
            HlsWeakNetworkState::CacheOnly
        } else if saw_upstream_failure {
            HlsWeakNetworkState::UpstreamFailed
        } else if degraded_session_count > 0 {
            HlsWeakNetworkState::Degraded
        } else if retrying_variant_count > 0 {
            HlsWeakNetworkState::Retrying
        } else {
            HlsWeakNetworkState::Normal
        };

        HlsWeakNetworkSnapshot {
            state,
            message: state.message().to_owned(),
            degraded_session_count,
            unhealthy_variant_count,
            retrying_variant_count,
            cache_only_session_count,
            last_changed_at: self.last_changed_at,
        }
    }
}

struct HlsSessionNetworkState {
    generation: u64,
    variants: HashMap<String, HlsVariantNetworkState>,
    cache_only_until: Option<SystemTime>,
}

impl HlsSessionNetworkState {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            variants: HashMap::new(),
            cache_only_until: None,
        }
    }

    fn has_degraded_variant(&self, now: SystemTime) -> bool {
        self.variants
            .values()
            .any(|variant| variant.is_degraded(now))
    }

    fn has_active_state(&self) -> bool {
        !self.variants.is_empty() || self.cache_only_until.is_some()
    }
}

#[derive(Default)]
struct HlsVariantNetworkState {
    consecutive_failures: u32,
    consecutive_slow_responses: u32,
    retrying_until: Option<SystemTime>,
    unhealthy_until: Option<SystemTime>,
    unhealthy_reason: Option<HlsWeakNetworkReason>,
    hold_degraded: bool,
}

impl HlsVariantNetworkState {
    fn is_degraded(&self, now: SystemTime) -> bool {
        self.hold_degraded || self.unhealthy_until.is_some_and(|until| until > now)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsWeakNetworkSnapshot {
    pub(crate) state: HlsWeakNetworkState,
    pub(crate) message: String,
    pub(crate) degraded_session_count: usize,
    pub(crate) unhealthy_variant_count: usize,
    pub(crate) retrying_variant_count: usize,
    pub(crate) cache_only_session_count: usize,
    pub(crate) last_changed_at: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsWeakNetworkState {
    Normal,
    Retrying,
    Degraded,
    CacheOnly,
    UpstreamFailed,
}

impl HlsWeakNetworkState {
    fn message(self) -> &'static str {
        match self {
            Self::Normal => "HLS upstream policy normal.",
            Self::Retrying => "Retrying HLS upstream requests via backup URLs.",
            Self::Degraded => "Weak upstream detected; advertising lower HLS variants temporarily.",
            Self::CacheOnly => "Serving HLS from local cache while upstream is degraded.",
            Self::UpstreamFailed => {
                "HLS upstream failed; playback may continue from cache when available."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsWeakNetworkReason {
    SlowUpstream,
    UpstreamFailed,
}

fn set_variant_degraded(
    variant: &mut HlsVariantNetworkState,
    weak_network_preference: WeakNetworkPreference,
    now: SystemTime,
) {
    if weak_network_preference == WeakNetworkPreference::HoldDowngrade {
        variant.hold_degraded = true;
        variant.unhealthy_until = None;
    } else {
        variant.unhealthy_until = Some(now + DEGRADE_DURATION);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_event_reports_retrying_without_hiding_variant() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_retry_at("session", "1080p", now);

        assert!(policy.variant_is_advertisable_at("session", "1080p", now));
        let snapshot = policy.snapshot_at(now);
        assert_eq!(HlsWeakNetworkState::Retrying, snapshot.state);
        assert_eq!(1, snapshot.retrying_variant_count);
        assert_eq!(0, snapshot.unhealthy_variant_count);
    }

    #[test]
    fn fast_success_preserves_retrying_window() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_retry_at("session", "1080p", now);
        policy.record_upstream_success_at(
            "session",
            "1080p",
            Duration::from_millis(50),
            now + Duration::from_secs(1),
        );

        assert!(policy.variant_is_advertisable_at(
            "session",
            "1080p",
            now + Duration::from_secs(1)
        ));
        let snapshot = policy.snapshot_at(now + Duration::from_secs(1));
        assert_eq!(HlsWeakNetworkState::Retrying, snapshot.state);
        assert_eq!(1, snapshot.retrying_variant_count);

        assert_eq!(
            HlsWeakNetworkState::Normal,
            policy
                .snapshot_at(now + RETRYING_WINDOW + Duration::from_secs(1))
                .state
        );
    }

    #[test]
    fn upstream_failure_temporarily_hides_variant_then_recovers() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_failure_at("session", "1080p", now);

        assert!(!policy.variant_is_advertisable_at("session", "1080p", now));
        let snapshot = policy.snapshot_at(now);
        assert_eq!(HlsWeakNetworkState::UpstreamFailed, snapshot.state);
        assert_eq!(1, snapshot.degraded_session_count);
        assert_eq!(1, snapshot.unhealthy_variant_count);

        let recovered_at = now + DEGRADE_DURATION + Duration::from_secs(1);
        assert!(policy.variant_is_advertisable_at("session", "1080p", recovered_at));
        assert_eq!(
            HlsWeakNetworkState::Normal,
            policy.snapshot_at(recovered_at).state
        );
    }

    #[test]
    fn hold_downgrade_policy_keeps_variant_hidden_until_session_removal() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_failure_at_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session",
            TEST_SESSION_GENERATION,
            "1080p",
            now,
        );

        let after_adaptive_recovery = now + DEGRADE_DURATION + Duration::from_secs(1);
        assert!(!policy.variant_is_advertisable_at_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session",
            TEST_SESSION_GENERATION,
            "1080p",
            after_adaptive_recovery,
        ));
        assert_eq!(
            HlsWeakNetworkState::UpstreamFailed,
            policy.snapshot_at(after_adaptive_recovery).state
        );

        policy.remove_session_at("session", after_adaptive_recovery);
        assert!(policy.variant_is_advertisable_at_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session",
            TEST_SESSION_GENERATION,
            "1080p",
            after_adaptive_recovery,
        ));
    }

    #[test]
    fn avplayer_managed_policy_does_not_accumulate_server_downgrade_state() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_failure_at_for_policy(
            WeakNetworkPreference::AvPlayerManaged,
            "session",
            TEST_SESSION_GENERATION,
            "1080p",
            now,
        );
        policy.record_upstream_success_at_for_policy(
            WeakNetworkPreference::AvPlayerManaged,
            "session",
            TEST_SESSION_GENERATION,
            "1080p",
            SLOW_RESPONSE_THRESHOLD,
            now,
        );

        assert!(policy.variant_is_advertisable_at_for_policy(
            WeakNetworkPreference::AvPlayerManaged,
            "session",
            TEST_SESSION_GENERATION,
            "1080p",
            now,
        ));
        assert_eq!(HlsWeakNetworkState::Normal, policy.snapshot_at(now).state);
    }

    #[test]
    fn fast_success_does_not_recover_degraded_variant_before_window() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_failure_at("session", "1080p", now);
        policy.record_upstream_success_at(
            "session",
            "1080p",
            Duration::from_millis(50),
            now + Duration::from_secs(1),
        );

        assert!(!policy.variant_is_advertisable_at(
            "session",
            "1080p",
            now + Duration::from_secs(1)
        ));
        assert_eq!(
            HlsWeakNetworkState::UpstreamFailed,
            policy.snapshot_at(now + Duration::from_secs(1)).state
        );

        let recovered_at = now + DEGRADE_DURATION + Duration::from_secs(1);
        assert!(policy.variant_is_advertisable_at("session", "1080p", recovered_at));
    }

    #[test]
    fn repeated_slow_success_degrades_variant() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_success_at("session", "1080p", SLOW_RESPONSE_THRESHOLD, now);
        assert!(policy.variant_is_advertisable_at("session", "1080p", now));
        assert_eq!(HlsWeakNetworkState::Retrying, policy.snapshot_at(now).state);

        policy.record_upstream_success_at(
            "session",
            "1080p",
            SLOW_RESPONSE_THRESHOLD + Duration::from_millis(1),
            now + Duration::from_secs(1),
        );

        assert!(!policy.variant_is_advertisable_at("session", "1080p", now));
        let snapshot = policy.snapshot_at(now + Duration::from_secs(1));
        assert_eq!(HlsWeakNetworkState::Degraded, snapshot.state);
        assert_eq!(1, snapshot.unhealthy_variant_count);
    }

    #[test]
    fn cache_hit_while_degraded_reports_cache_only() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_failure_at("session", "1080p", now);
        policy.record_cache_hit_at("session", now + Duration::from_secs(1));

        let snapshot = policy.snapshot_at(now + Duration::from_secs(1));
        assert_eq!(HlsWeakNetworkState::CacheOnly, snapshot.state);
        assert_eq!(1, snapshot.cache_only_session_count);
    }

    #[test]
    fn cache_only_recovers_when_degraded_variant_recovers() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_failure_at("session", "1080p", now);
        policy.record_cache_hit_at("session", now + DEGRADE_DURATION - Duration::from_secs(1));

        assert_eq!(
            HlsWeakNetworkState::CacheOnly,
            policy
                .snapshot_at(now + DEGRADE_DURATION - Duration::from_secs(1))
                .state
        );

        let recovered_at = now + DEGRADE_DURATION + Duration::from_secs(1);
        let snapshot = policy.snapshot_at(recovered_at);
        assert_eq!(HlsWeakNetworkState::Normal, snapshot.state);
        assert_eq!(0, snapshot.cache_only_session_count);
        assert_eq!(0, snapshot.degraded_session_count);
    }

    #[test]
    fn remove_session_clears_active_state() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.record_upstream_failure_at("session", "1080p", now);
        let active = policy.snapshot_at(now + Duration::from_secs(1));
        assert_eq!(HlsWeakNetworkState::UpstreamFailed, active.state);

        policy.remove_session_at("session", now + Duration::from_secs(2));

        let snapshot = policy.snapshot_at(now + Duration::from_secs(3));
        assert_eq!(HlsWeakNetworkState::Normal, snapshot.state);
        assert_eq!(0, snapshot.retrying_variant_count);
        assert_eq!(0, snapshot.degraded_session_count);
        assert_eq!(0, snapshot.unhealthy_variant_count);
        assert_eq!(0, snapshot.cache_only_session_count);
    }

    #[test]
    fn stale_generation_cannot_modify_or_remove_newer_state() {
        let policy = HlsNetworkPolicy::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        policy.advance_session_generation_at("session", 1, now);
        policy.record_upstream_failure_at_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session",
            1,
            "1080p",
            now,
        );
        assert_eq!(
            HlsWeakNetworkState::UpstreamFailed,
            policy.snapshot_at(now).state
        );

        let next_generation_at = now + Duration::from_secs(1);
        policy.advance_session_generation_at("session", 2, next_generation_at);
        policy.record_upstream_failure_at_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session",
            1,
            "stale-1080p",
            next_generation_at,
        );
        assert_eq!(
            HlsWeakNetworkState::Normal,
            policy.snapshot_at(next_generation_at).state
        );

        policy.record_upstream_failure_at_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session",
            2,
            "720p",
            next_generation_at,
        );
        policy.remove_session_generation_at("session", 1, next_generation_at);
        assert_eq!(
            HlsWeakNetworkState::UpstreamFailed,
            policy.snapshot_at(next_generation_at).state
        );
        assert!(policy.variant_is_advertisable_at_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session",
            1,
            "720p",
            next_generation_at,
        ));
        assert!(!policy.variant_is_advertisable_at_for_policy(
            WeakNetworkPreference::HoldDowngrade,
            "session",
            2,
            "720p",
            next_generation_at,
        ));

        policy.remove_session_generation_at("session", 2, next_generation_at);
        assert_eq!(
            HlsWeakNetworkState::Normal,
            policy.snapshot_at(next_generation_at).state
        );
    }
}
