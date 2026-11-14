use super::*;
use crate::playback_request_urgency::PlaybackSidxRange;

fn sample(
    playhead_seconds: f64,
    buffered_end_seconds: f64,
    playback_rate: f64,
) -> PlaybackTimingSample {
    PlaybackTimingSample {
        playhead_seconds,
        continuous_buffered_end_seconds: buffered_end_seconds,
        playback_rate,
    }
}

#[test]
fn safe_future_start_requires_wall_time_headroom_and_keeps_real_buffer_end() {
    let cases = [
        (5.0, 25.0, 1.0, Some(25.0)),
        (5.0, 26.0, 1.0, Some(26.0)),
        (5.0, 40.0, 0.5, Some(40.0)),
        (2.0, 322.0, 16.0, Some(322.0)),
        (0.0, 19.999, 1.0, None),
    ];

    for (playhead, buffered_end, rate, expected) in cases {
        assert_eq!(
            safe_future_start(sample(playhead, buffered_end, rate)),
            expected,
            "playhead={playhead}, buffered_end={buffered_end}, rate={rate}"
        );
    }
}

#[test]
fn safe_future_start_rejects_invalid_timing_and_rates() {
    let cases = [
        sample(-1.0, 20.0, 1.0),
        sample(f64::NAN, 20.0, 1.0),
        sample(0.0, f64::INFINITY, 1.0),
        sample(5.0, 4.0, 1.0),
        sample(0.0, 20.0, 0.0),
        sample(0.0, 20.0, -1.0),
        sample(0.0, 20.0, f64::NAN),
        sample(0.0, 20.0, f64::INFINITY),
        sample(0.0, 320.0, 16.0001),
    ];

    for invalid in cases {
        assert_eq!(
            safe_future_start(invalid),
            None,
            "invalid sample: {invalid:?}"
        );
    }
}

#[test]
fn optional_preparation_supply_requires_rate_scaled_headroom() {
    let cases = [
        (1_500_002, 1_000_001, 1.0, true),
        (1_500_001, 1_000_001, 1.0, false),
        (3_000_000, 1_000_000, 2.0, true),
        (2_999_999, 1_000_000, 2.0, false),
        (0, 1_000_000, 1.0, false),
        (1_500_000, 1_000_000, 0.0, false),
        (1_500_000, 1_000_000, 16.1, false),
        (u64::MAX, u64::MAX, 1.0, false),
    ];

    for (throughput, nominal, rate, expected) in cases {
        assert_eq!(
            optional_preparation_supply_sufficient(throughput, nominal, rate),
            expected,
            "throughput={throughput}, nominal={nominal}, rate={rate}"
        );
    }
}

fn sidx_map(boundaries: &[u64]) -> PlaybackSidxMap {
    let ranges = boundaries
        .windows(2)
        .enumerate()
        .map(|(index, pair)| {
            let byte_start = 100 + index as u64 * 100;
            PlaybackSidxRange {
                bytes: byte_start..byte_start + 100,
                start_ticks: pair[0],
                end_ticks: pair[1],
                timescale: 1,
            }
        })
        .collect::<Vec<_>>();
    let total_length = 100 + ranges.len() as u64 * 100;
    PlaybackSidxMap::new(total_length, ranges).expect("fixture SIDX map should be valid")
}

#[test]
fn preparation_ranges_cover_the_same_window_on_independent_fragment_boundaries() {
    let cases = [
        (
            vec![0, 10, 20, 30, 40, 50],
            vec![0, 7, 22, 37, 52],
            15.0,
            35.0,
            (10.0, 40.0, 200..500),
            (7.0, 37.0, 200..400),
        ),
        (
            vec![0, 10, 20, 30, 40, 50],
            vec![0, 7, 22, 37, 52],
            22.0,
            35.0,
            (20.0, 40.0, 300..500),
            (22.0, 37.0, 300..400),
        ),
    ];

    for (
        video_boundaries,
        audio_boundaries,
        window_start,
        window_end,
        video_expected,
        audio_expected,
    ) in cases
    {
        let video = sidx_map(&video_boundaries)
            .preparation_range_for_window(window_start, window_end)
            .expect("video fragments should cover window");
        let audio = sidx_map(&audio_boundaries)
            .preparation_range_for_window(window_start, window_end)
            .expect("audio fragments should cover window");

        assert_eq!(
            (video.start_seconds, video.end_seconds, video.bytes.clone(),),
            video_expected
        );
        assert_eq!(
            (audio.start_seconds, audio.end_seconds, audio.bytes.clone(),),
            audio_expected
        );
        assert!(video.start_seconds <= window_start && video.end_seconds >= window_end);
        assert!(audio.start_seconds <= window_start && audio.end_seconds >= window_end);
        assert!(video.bytes.start >= 100 && audio.bytes.start >= 100);
    }
}

fn plan_observation(
    plan_id: u64,
    reader_id: &str,
    outcome: HighPreparationPlanOutcome,
) -> HighPreparationPlanObservation {
    HighPreparationPlanObservation {
        plan_id,
        context_id: Some(format!("context-{plan_id}")),
        http_reader_id: Some(reader_id.to_owned()),
        session_id: Some("session".to_owned()),
        session_generation: Some(1),
        target_variant_id: Some("variant".to_owned()),
        window_start_seconds: Some(20.0),
        window_end_seconds: Some(40.0),
        initial_window_readable: Some(false),
        selected_spans: Vec::new(),
        outcome,
        defer_reason: None,
        requested_resource_ranges: 0,
        readable_resource_ranges: 0,
        deferred_resource_ranges: 0,
        window_readable: false,
    }
}

#[test]
fn active_plan_observations_and_counters_survive_defer_history_churn() {
    let coordinator = HighPreparationCoordinator::default();
    for plan_id in [1_001, 1_002] {
        coordinator.record_admitted(plan_observation(
            plan_id,
            &format!("active-reader-{plan_id}"),
            HighPreparationPlanOutcome::Admitted,
        ));
        coordinator.record_requested_resource_ranges(plan_id, 1);
    }

    for plan_id in 0..24 {
        let mut deferred = plan_observation(
            100 + plan_id,
            &format!("other-reader-{plan_id}"),
            HighPreparationPlanOutcome::Admitted,
        );
        deferred.requested_resource_ranges = 1;
        coordinator.record_deferred(
            deferred,
            HighPreparationDeferReason::Cooldown,
            Duration::ZERO,
        );
    }

    {
        let state = coordinator
            .inner
            .state
            .lock()
            .expect("coordinator lock should remain available");
        assert_eq!(state.active_plans.len(), 2);
    }

    for plan_id in [1_001, 1_002] {
        coordinator.finish_plan(
            plan_id,
            HighPreparationPlanOutcome::Completed,
            None,
            1,
            true,
            &[],
        );
    }

    let observation = coordinator.observation_for_tests();
    assert_eq!(
        observation.plan_history.len(),
        HIGH_PREPARATION_HISTORY_LIMIT
    );
    assert_eq!(observation.admitted_plans, 2);
    assert_eq!(observation.completed_plans, 2);
    assert_eq!(observation.deferred_plans, 24);
    assert_eq!(observation.requested_resource_ranges, 2);
    assert_eq!(observation.readable_resource_ranges, 2);
    for plan_id in [1_001, 1_002] {
        let plan = observation
            .plan_history
            .iter()
            .find(|plan| plan.plan_id == plan_id)
            .expect("active plan history should be retained");
        assert_eq!(plan.outcome, HighPreparationPlanOutcome::Completed);
        assert_eq!(plan.requested_resource_ranges, 1);
        assert_eq!(plan.readable_resource_ranges, 1);
    }
}
