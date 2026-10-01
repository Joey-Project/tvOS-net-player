use std::{collections::HashMap, time::Instant};

use crate::resolver_settings::normalize_resolver_host_id;

const MAX_HOSTS: usize = 128;
const RECORD_TTL_SECS: u64 = 24 * 60 * 60;
const BASE_COOLDOWN_SECS: u64 = 5;
const MAX_COOLDOWN_SECS: u64 = 5 * 60;
const EWMA_SAMPLE_LIMIT_MICROS: u128 = 60_000_000;
const PROBE_REFRESH_SECS: u64 = 10 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum ProbeFailure {
    Connectivity,
    Tls,
    Timeout,
    Authentication,
    RegionContent,
    HttpStatus(u16),
}

#[derive(Clone, Debug)]
struct HostHealth {
    last_probe: Option<Instant>,
    last_success: Option<Instant>,
    last_failure: Option<Instant>,
    latest_outcome: Option<ProbeOutcome>,
    revalidation_due: bool,
    last_updated: Instant,
    first_response_ewma_micros: Option<u64>,
    consecutive_connectivity_failures: u32,
    cooldown_until: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeOutcome {
    Success,
    TransportFailure,
}

#[derive(Default)]
pub(crate) struct ResolverHealthTracker {
    hosts: HashMap<String, HostHealth>,
}

impl ResolverHealthTracker {
    pub(crate) fn probe_due(&mut self, host_id: &str, now: Instant) -> bool {
        self.expire_old(now);
        let Some(health) = self.hosts.get(host_id) else {
            return true;
        };
        if health.cooldown_until.is_some_and(|until| now < until) {
            return false;
        }
        if health.revalidation_due {
            return true;
        }
        health.last_probe.is_none_or(|probe| {
            now.checked_duration_since(probe)
                .is_none_or(|age| age.as_secs() >= PROBE_REFRESH_SECS)
        })
    }

    pub(crate) fn recently_healthy(&mut self, host_id: &str, now: Instant) -> bool {
        self.expire_old(now);
        self.hosts.get(host_id).is_some_and(|health| {
            health.latest_outcome == Some(ProbeOutcome::Success)
                && health.last_success.is_some_and(|success| {
                    now.checked_duration_since(success)
                        .is_some_and(|age| age.as_secs() < PROBE_REFRESH_SECS)
                })
                && health.cooldown_until.is_none_or(|until| now >= until)
        })
    }

    pub(crate) fn record_success(
        &mut self,
        host_id: &str,
        now: Instant,
        first_response: std::time::Duration,
    ) -> bool {
        let Ok(host_id) = validate_host_id(host_id) else {
            return false;
        };
        self.expire_old(now);
        self.ensure_capacity(&host_id);
        let entry = self.hosts.entry(host_id).or_insert_with(|| HostHealth {
            last_probe: None,
            last_success: None,
            last_failure: None,
            latest_outcome: None,
            revalidation_due: false,
            last_updated: now,
            first_response_ewma_micros: None,
            consecutive_connectivity_failures: 0,
            cooldown_until: None,
        });
        let sample = first_response.as_micros().min(EWMA_SAMPLE_LIMIT_MICROS) as u64;
        entry.first_response_ewma_micros = Some(match entry.first_response_ewma_micros {
            Some(previous) => ((u128::from(previous) * 7 + u128::from(sample)) / 8) as u64,
            None => sample,
        });
        entry.last_probe = Some(now);
        entry.last_success = Some(now);
        entry.latest_outcome = Some(ProbeOutcome::Success);
        entry.revalidation_due = false;
        entry.last_updated = now;
        entry.consecutive_connectivity_failures = 0;
        entry.cooldown_until = None;
        true
    }

    pub(crate) fn record_failure(
        &mut self,
        host_id: &str,
        now: Instant,
        failure: ProbeFailure,
    ) -> bool {
        let Ok(host_id) = validate_host_id(host_id) else {
            return false;
        };
        if !matches!(
            failure,
            ProbeFailure::Connectivity | ProbeFailure::Tls | ProbeFailure::Timeout
        ) {
            return false;
        }
        self.expire_old(now);
        self.ensure_capacity(&host_id);
        let entry = self.hosts.entry(host_id).or_insert_with(|| HostHealth {
            last_probe: None,
            last_success: None,
            last_failure: None,
            latest_outcome: None,
            revalidation_due: false,
            last_updated: now,
            first_response_ewma_micros: None,
            consecutive_connectivity_failures: 0,
            cooldown_until: None,
        });
        entry.last_probe = Some(now);
        entry.last_failure = Some(now);
        entry.latest_outcome = Some(ProbeOutcome::TransportFailure);
        entry.revalidation_due = true;
        entry.last_updated = now;
        entry.consecutive_connectivity_failures =
            entry.consecutive_connectivity_failures.saturating_add(1);
        let exponent = entry
            .consecutive_connectivity_failures
            .saturating_sub(1)
            .min(6);
        let cooldown_secs = (BASE_COOLDOWN_SECS * (1_u64 << exponent)).min(MAX_COOLDOWN_SECS);
        entry.cooldown_until = now.checked_add(std::time::Duration::from_secs(cooldown_secs));
        true
    }

    pub(crate) fn record_neutral_probe(&mut self, host_id: &str, now: Instant) -> bool {
        let Ok(host_id) = validate_host_id(host_id) else {
            return false;
        };
        self.expire_old(now);
        self.ensure_capacity(&host_id);
        let entry = self.hosts.entry(host_id).or_insert_with(|| HostHealth {
            last_probe: None,
            last_success: None,
            last_failure: None,
            latest_outcome: None,
            revalidation_due: false,
            last_updated: now,
            first_response_ewma_micros: None,
            consecutive_connectivity_failures: 0,
            cooldown_until: None,
        });
        entry.last_probe = Some(now);
        entry.revalidation_due = false;
        entry.last_updated = now;
        true
    }

    pub(crate) fn rank_candidates(&mut self, candidates: &mut [String], now: Instant) {
        self.expire_old(now);
        candidates.sort_by(|left, right| {
            let left_health = self.hosts.get(left);
            let right_health = self.hosts.get(right);
            rank_key(left_health, now).cmp(&rank_key(right_health, now))
        });
    }

    fn expire_old(&mut self, now: Instant) {
        self.hosts.retain(|_, health| {
            now.checked_duration_since(health.last_updated)
                .is_none_or(|age| age.as_secs() < RECORD_TTL_SECS)
        });
    }

    fn ensure_capacity(&mut self, host_id: &str) {
        if self.hosts.contains_key(host_id) || self.hosts.len() < MAX_HOSTS {
            return;
        }
        if let Some(oldest) = self
            .hosts
            .iter()
            .min_by_key(|(_, health)| health.last_updated)
            .map(|(host, _)| host.clone())
        {
            self.hosts.remove(&oldest);
        }
    }
}

fn rank_key(health: Option<&HostHealth>, now: Instant) -> (bool, u8, u64, u64) {
    match health {
        Some(health) => {
            let cooling = health.cooldown_until.is_some_and(|until| now < until);
            let success_age = health
                .last_success
                .and_then(|time| now.checked_duration_since(time))
                .map(|age| age.as_secs())
                .unwrap_or(u64::MAX);
            let quality = if health.last_success.is_some() { 0 } else { 2 };
            (
                cooling,
                quality,
                success_age,
                health.first_response_ewma_micros.unwrap_or(u64::MAX),
            )
        }
        None => (false, 1, u64::MAX, u64::MAX),
    }
}

fn validate_host_id(host_id: &str) -> Result<String, ()> {
    normalize_resolver_host_id(host_id).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_HOSTS, PROBE_REFRESH_SECS, ProbeFailure, RECORD_TTL_SECS, ResolverHealthTracker,
    };
    use std::time::{Duration, Instant};

    fn host(name: &str) -> String {
        format!("{name}.example")
    }

    #[test]
    fn ranks_successful_low_latency_hosts_before_unknown_and_cooling_hosts() {
        let now = Instant::now();
        let mut tracker = ResolverHealthTracker::default();
        tracker.record_success("fast.example", now, Duration::from_millis(20));
        tracker.record_success("slow.example", now, Duration::from_millis(400));
        tracker.record_failure("cool.example", now, ProbeFailure::Timeout);
        let mut candidates = vec![
            "cool.example".into(),
            "unknown.example".into(),
            "slow.example".into(),
            "fast.example".into(),
        ];
        tracker.rank_candidates(&mut candidates, now);
        assert_eq!(
            candidates,
            [
                "fast.example",
                "slow.example",
                "unknown.example",
                "cool.example"
            ]
        );
    }

    #[test]
    fn accepts_canonical_custom_https_ports() {
        let now = Instant::now();
        let mut tracker = ResolverHealthTracker::default();
        assert!(tracker.record_success("custom.example:8443", now, Duration::from_millis(5)));
        assert!(!tracker.record_success("custom.example:443", now, Duration::from_millis(5)));
        let mut candidates = vec!["custom.example:8443".into(), "other.example".into()];
        tracker.rank_candidates(&mut candidates, now);
        assert_eq!(candidates[0], "custom.example:8443");
    }

    #[test]
    fn cooldown_expires_without_sleeping() {
        let now = Instant::now();
        let mut tracker = ResolverHealthTracker::default();
        tracker.record_failure("offline.example", now, ProbeFailure::Connectivity);
        let mut candidates = vec!["offline.example".into(), "unknown.example".into()];
        tracker.rank_candidates(&mut candidates, now);
        assert_eq!(candidates[0], "unknown.example");
        candidates = vec!["offline.example".into(), "unknown.example".into()];
        tracker.rank_candidates(&mut candidates, now + Duration::from_secs(6));
        assert_eq!(candidates[0], "unknown.example");
    }

    #[test]
    fn only_connectivity_tls_and_timeout_failures_train_health() {
        let now = Instant::now();
        let mut tracker = ResolverHealthTracker::default();
        for failure in [
            ProbeFailure::Authentication,
            ProbeFailure::RegionContent,
            ProbeFailure::HttpStatus(503),
        ] {
            assert!(!tracker.record_failure("not-a-failure.example", now, failure));
        }
        assert!(tracker.hosts.is_empty());
        assert!(tracker.record_failure("real-failure.example", now, ProbeFailure::Tls));
    }

    #[test]
    fn connectivity_failures_invalidate_cached_success_and_are_reprobed_after_cooldown() {
        for failure in [
            ProbeFailure::Connectivity,
            ProbeFailure::Tls,
            ProbeFailure::Timeout,
        ] {
            let now = Instant::now();
            let mut tracker = ResolverHealthTracker::default();
            tracker.record_success("resolver.example", now, Duration::from_millis(10));
            assert!(tracker.recently_healthy("resolver.example", now));
            assert!(!tracker.probe_due("resolver.example", now));

            tracker.record_failure("resolver.example", now, failure);
            assert!(!tracker.recently_healthy("resolver.example", now));
            assert!(!tracker.probe_due("resolver.example", now + Duration::from_secs(4)));
            assert!(tracker.probe_due("resolver.example", now + Duration::from_secs(5)));

            let revalidated_at = now + Duration::from_secs(5);
            tracker.record_success(
                "resolver.example",
                revalidated_at,
                Duration::from_millis(12),
            );
            assert!(tracker.recently_healthy("resolver.example", revalidated_at));
            assert!(!tracker.probe_due("resolver.example", revalidated_at));
            assert!(tracker.probe_due(
                "resolver.example",
                revalidated_at + Duration::from_secs(PROBE_REFRESH_SECS)
            ));
        }
    }

    #[test]
    fn same_instant_outcomes_follow_recording_order() {
        let now = Instant::now();
        let mut tracker = ResolverHealthTracker::default();
        tracker.record_success("resolver.example", now, Duration::from_millis(10));
        tracker.record_failure("resolver.example", now, ProbeFailure::Connectivity);
        assert!(!tracker.recently_healthy("resolver.example", now));
        assert!(tracker.probe_due("resolver.example", now + Duration::from_secs(5)));

        tracker.record_success("resolver.example", now, Duration::from_millis(12));
        assert!(tracker.recently_healthy("resolver.example", now));
        assert!(!tracker.probe_due("resolver.example", now));
    }

    #[test]
    fn neutral_probe_preserves_failure_invalidation_and_refresh_interval() {
        let now = Instant::now();
        let mut tracker = ResolverHealthTracker::default();
        tracker.record_success("resolver.example", now, Duration::from_millis(10));
        tracker.record_failure("resolver.example", now, ProbeFailure::Connectivity);
        let neutral_probe_at = now + Duration::from_secs(5);
        tracker.record_neutral_probe("resolver.example", neutral_probe_at);

        assert!(!tracker.recently_healthy("resolver.example", neutral_probe_at));
        assert!(!tracker.probe_due(
            "resolver.example",
            neutral_probe_at + Duration::from_secs(PROBE_REFRESH_SECS - 1)
        ));
        assert!(tracker.probe_due(
            "resolver.example",
            neutral_probe_at + Duration::from_secs(PROBE_REFRESH_SECS)
        ));
    }

    #[test]
    fn records_are_bounded_and_expire() {
        let now = Instant::now();
        let mut tracker = ResolverHealthTracker::default();
        for index in 0..(MAX_HOSTS + 1) {
            assert!(tracker.record_success(
                &host(&format!("h{index}")),
                now,
                Duration::from_millis(5)
            ));
        }
        assert_eq!(tracker.hosts.len(), MAX_HOSTS);
        tracker.rank_candidates(&mut [], now + Duration::from_secs(RECORD_TTL_SECS));
        assert!(tracker.hosts.is_empty());
    }

    #[test]
    fn rejects_urls_and_never_retains_secret_material() {
        let now = Instant::now();
        let mut tracker = ResolverHealthTracker::default();
        for invalid in [
            "https://user:secret@example.com/?token=secret",
            "Example.com",
            "example.com:443",
            "example.com/path",
        ] {
            assert!(!tracker.record_success(invalid, now, Duration::from_millis(1)));
        }
        assert!(tracker.record_success("safe.example", now, Duration::from_millis(1)));
        assert_eq!(tracker.hosts.len(), 1);
        assert!(
            tracker
                .hosts
                .keys()
                .all(|key| !key.contains("secret") && !key.contains("https://"))
        );
    }
}
