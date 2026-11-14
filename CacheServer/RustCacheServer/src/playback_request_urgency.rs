use std::{
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    AppState,
    hls::HlsMediaResource,
    hls_playback_context::{PlaybackTimingSample, is_canonical_uuid_v4},
    hls_range_cache::{HlsRangePriority, HlsRangePriorityResolver},
};

pub(crate) const PLAYBACK_FAR_FUTURE_WAIT_AGE: Duration = Duration::from_secs(2);
const PLAYBACK_PROTECTED_WALL_SECONDS: f64 = 20.0;
const MAX_PLAYBACK_SIDX_RANGES: usize = 16_384;
const MAX_PLAYBACK_SIDX_MAP_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlaybackSidxRange {
    pub(crate) bytes: Range<u64>,
    pub(crate) start_ticks: u64,
    pub(crate) end_ticks: u64,
    pub(crate) timescale: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlaybackSidxMap {
    total_length: u64,
    ranges: Vec<PlaybackSidxRange>,
}

pub(crate) struct PlaybackRequestUrgency {
    state: Arc<AppState>,
    reader_id: String,
    session_id: String,
    session_generation: u64,
    resource: HlsMediaResource,
    pending_chunk_age: Mutex<PendingChunkAge>,
}

#[derive(Default)]
struct PendingChunkAge {
    current: Option<(Range<u64>, Instant)>,
}

impl PendingChunkAge {
    fn elapsed_for(&mut self, range: &Range<u64>, now: Instant) -> Duration {
        if let Some((current, started_at)) = &self.current
            && current == range
        {
            return now.saturating_duration_since(*started_at);
        }
        self.current = Some((range.clone(), now));
        Duration::ZERO
    }
}

impl PlaybackRequestUrgency {
    pub(crate) fn new(
        state: Arc<AppState>,
        reader_id: &str,
        session_id: &str,
        session_generation: u64,
        resource: HlsMediaResource,
    ) -> Option<Self> {
        if !is_canonical_uuid_v4(reader_id) || session_generation == 0 {
            return None;
        }
        Some(Self {
            state,
            reader_id: reader_id.to_owned(),
            session_id: session_id.to_owned(),
            session_generation,
            resource,
            pending_chunk_age: Mutex::new(PendingChunkAge::default()),
        })
    }

    pub(crate) fn priority_for_range(
        &self,
        range: &Range<u64>,
        waited: Duration,
    ) -> HlsRangePriority {
        // This resolver spans one HTTP GET: the same chunk is a retry, a new chunk follows progress.
        let elapsed = self
            .pending_chunk_age
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed_for(range, Instant::now());
        let waited = waited.max(elapsed);
        let Some(snapshot) = self
            .state
            .current_hls_playback_context_for_reader(&self.reader_id)
        else {
            return HlsRangePriority::Foreground;
        };
        if snapshot.session_id != self.session_id
            || snapshot.session_generation != self.session_generation
        {
            return HlsRangePriority::Foreground;
        }
        let Some(map) = self.state.hls_cache.playback_sidx_map(
            &self.session_id,
            self.session_generation,
            &self.resource,
        ) else {
            return HlsRangePriority::Foreground;
        };
        let Some(required_bitrate_bps) = self.resource.request.bandwidth else {
            return HlsRangePriority::Foreground;
        };
        let Some(origin_supply) = self
            .state
            .cdn_history
            .recent_origin_supply(&self.resource.request)
        else {
            return HlsRangePriority::Foreground;
        };
        if !origin_supply_supports_playback_rate(
            origin_supply.throughput_bps,
            required_bitrate_bps,
            snapshot.sample.playback_rate,
        ) {
            return HlsRangePriority::Foreground;
        }
        map.priority_for_range(range, snapshot.sample, waited)
    }

    pub(crate) fn resolver(self: Arc<Self>) -> HlsRangePriorityResolver {
        Arc::new(move |range, waited| self.priority_for_range(range, waited))
    }
}

fn origin_supply_supports_playback_rate(
    throughput_bps: u64,
    nominal_bitrate_bps: u64,
    playback_rate: f64,
) -> bool {
    if throughput_bps == 0
        || nominal_bitrate_bps == 0
        || !playback_rate.is_finite()
        || playback_rate <= 0.0
        || playback_rate > 16.0
    {
        return false;
    }

    let required_bitrate_bps = nominal_bitrate_bps as f64 * playback_rate;
    if !required_bitrate_bps.is_finite() || required_bitrate_bps >= u64::MAX as f64 {
        return false;
    }
    throughput_bps >= required_bitrate_bps.ceil() as u64
}

impl PlaybackSidxMap {
    pub(crate) fn new(total_length: u64, ranges: Vec<PlaybackSidxRange>) -> Option<Self> {
        if total_length == 0
            || ranges.len() < 2
            || ranges.len() > MAX_PLAYBACK_SIDX_RANGES
            || ranges
                .capacity()
                .checked_mul(std::mem::size_of::<PlaybackSidxRange>())?
                > MAX_PLAYBACK_SIDX_MAP_BYTES
        {
            return None;
        }
        let first = ranges.first()?;
        let timescale = first.timescale;
        if timescale == 0 || first.start_ticks != 0 {
            return None;
        }
        let mut previous_byte = first.bytes.start;
        let mut previous_time = first.start_ticks;
        for range in &ranges {
            if range.bytes.start != previous_byte
                || range.bytes.start >= range.bytes.end
                || range.bytes.end > total_length
                || range.start_ticks != previous_time
                || range.start_ticks >= range.end_ticks
                || range.timescale != timescale
            {
                return None;
            }
            previous_byte = range.bytes.end;
            previous_time = range.end_ticks;
        }
        Some(Self {
            total_length,
            ranges,
        })
    }

    pub(crate) fn priority_for_range(
        &self,
        requested: &Range<u64>,
        sample: PlaybackTimingSample,
        waited: Duration,
    ) -> HlsRangePriority {
        if waited >= PLAYBACK_FAR_FUTURE_WAIT_AGE
            || requested.start >= requested.end
            || requested.end > self.total_length
            || !sample.playhead_seconds.is_finite()
            || sample.playhead_seconds < 0.0
            || !sample.continuous_buffered_end_seconds.is_finite()
            || sample.continuous_buffered_end_seconds < sample.playhead_seconds
            || !sample.playback_rate.is_finite()
            || sample.playback_rate <= 0.0
            || sample.playback_rate > 16.0
        {
            return HlsRangePriority::Foreground;
        }

        let protected_end =
            sample.playhead_seconds + PLAYBACK_PROTECTED_WALL_SECONDS * sample.playback_rate;
        if !protected_end.is_finite() || sample.continuous_buffered_end_seconds < protected_end {
            return HlsRangePriority::Foreground;
        }

        let mut cursor = requested.start;
        for range in &self.ranges {
            if range.bytes.end <= cursor || range.bytes.start >= requested.end {
                continue;
            }
            if range.bytes.start > cursor {
                return HlsRangePriority::Foreground;
            }
            let segment_start_seconds = range.start_ticks as f64 / range.timescale as f64;
            if !segment_start_seconds.is_finite() || segment_start_seconds <= protected_end {
                return HlsRangePriority::Foreground;
            }
            cursor = range.bytes.end.min(requested.end);
            if cursor == requested.end {
                return HlsRangePriority::FarFuturePlayback;
            }
        }
        HlsRangePriority::Foreground
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.ranges
            .capacity()
            .saturating_mul(std::mem::size_of::<PlaybackSidxRange>())
    }

    pub(crate) fn total_length(&self) -> u64 {
        self.total_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> PlaybackSidxMap {
        PlaybackSidxMap::new(
            400,
            vec![
                PlaybackSidxRange {
                    bytes: 100..200,
                    start_ticks: 0,
                    end_ticks: 10_000,
                    timescale: 1_000,
                },
                PlaybackSidxRange {
                    bytes: 200..300,
                    start_ticks: 10_000,
                    end_ticks: 20_000,
                    timescale: 1_000,
                },
                PlaybackSidxRange {
                    bytes: 300..350,
                    start_ticks: 20_000,
                    end_ticks: 30_000,
                    timescale: 1_000,
                },
                PlaybackSidxRange {
                    bytes: 350..400,
                    start_ticks: 30_000,
                    end_ticks: 40_000,
                    timescale: 1_000,
                },
            ],
        )
        .expect("fixture map should be contiguous")
    }

    fn sample(playhead: f64, buffered_end: f64, rate: f64) -> PlaybackTimingSample {
        PlaybackTimingSample {
            playhead_seconds: playhead,
            continuous_buffered_end_seconds: buffered_end,
            playback_rate: rate,
        }
    }

    #[test]
    fn protects_playhead_wall_window_and_requires_contiguous_buffer_coverage() {
        let map = map();
        assert_eq!(
            HlsRangePriority::Foreground,
            map.priority_for_range(&(100..110), sample(0.0, 20.0, 1.0), Duration::ZERO)
        );
        assert_eq!(
            HlsRangePriority::FarFuturePlayback,
            map.priority_for_range(&(350..360), sample(0.0, 20.0, 1.0), Duration::ZERO)
        );
        assert_eq!(
            HlsRangePriority::Foreground,
            map.priority_for_range(&(300..310), sample(0.0, 20.0, 1.0), Duration::ZERO)
        );
        assert_eq!(
            HlsRangePriority::Foreground,
            map.priority_for_range(&(350..360), sample(0.0, 19.999, 1.0), Duration::ZERO)
        );
        assert_eq!(
            HlsRangePriority::Foreground,
            map.priority_for_range(&(350..360), sample(0.0, 40.0, 2.0), Duration::ZERO)
        );
    }

    #[test]
    fn origin_supply_must_cover_rate_scaled_nominal_bitrate() {
        assert!(!origin_supply_supports_playback_rate(
            1_500_000, 1_000_000, 2.0
        ));
        assert!(origin_supply_supports_playback_rate(
            2_000_000, 1_000_000, 2.0
        ));
        assert!(!origin_supply_supports_playback_rate(0, 1_000_000, 1.0));
        for invalid_rate in [0.0, -1.0, f64::NAN, f64::INFINITY, 16.1] {
            assert!(!origin_supply_supports_playback_rate(
                2_000_000,
                1_000_000,
                invalid_rate
            ));
        }
        assert!(!origin_supply_supports_playback_rate(
            u64::MAX,
            u64::MAX,
            2.0
        ));
    }

    #[test]
    fn mixed_or_unmapped_chunks_keep_continuity_priority() {
        let map = map();
        assert_eq!(
            HlsRangePriority::Foreground,
            map.priority_for_range(&(190..210), sample(0.0, 20.0, 1.0), Duration::ZERO)
        );
        assert_eq!(
            HlsRangePriority::Foreground,
            map.priority_for_range(&(400..410), sample(0.0, 20.0, 1.0), Duration::ZERO)
        );
        assert_eq!(
            HlsRangePriority::Foreground,
            map.priority_for_range(&(0..10), sample(0.0, 20.0, 1.0), Duration::ZERO)
        );
    }

    #[test]
    fn pending_far_future_work_ages_to_urgent_without_changing_map() {
        let map = map();
        assert_eq!(
            HlsRangePriority::FarFuturePlayback,
            map.priority_for_range(
                &(350..360),
                sample(0.0, 20.0, 1.0),
                Duration::from_millis(1_999)
            )
        );
        assert_eq!(
            HlsRangePriority::Foreground,
            map.priority_for_range(&(350..360), sample(0.0, 20.0, 1.0), Duration::from_secs(2))
        );
    }

    #[test]
    fn invalid_or_ambiguous_timing_map_is_rejected() {
        assert!(PlaybackSidxMap::new(400, Vec::new()).is_none());
        assert!(
            PlaybackSidxMap::new(
                400,
                vec![
                    PlaybackSidxRange {
                        bytes: 100..200,
                        start_ticks: 0,
                        end_ticks: 10,
                        timescale: 1_000,
                    },
                    PlaybackSidxRange {
                        bytes: 201..300,
                        start_ticks: 10,
                        end_ticks: 20,
                        timescale: 1_000,
                    },
                ],
            )
            .is_none()
        );
    }

    #[test]
    fn nonzero_sidx_earliest_time_is_not_priority_eligible() {
        let mut ranges = vec![
            PlaybackSidxRange {
                bytes: 100..200,
                start_ticks: 5_000,
                end_ticks: 15_000,
                timescale: 1_000,
            },
            PlaybackSidxRange {
                bytes: 200..300,
                start_ticks: 15_000,
                end_ticks: 25_000,
                timescale: 1_000,
            },
        ];
        ranges.shrink_to_fit();
        assert!(PlaybackSidxMap::new(400, ranges).is_none());
    }

    #[test]
    fn pending_chunk_age_survives_retry_and_resets_after_progress() {
        let mut age = PendingChunkAge::default();
        let started = Instant::now();
        let first = 64..128;
        let next = 128..192;

        assert_eq!(Duration::ZERO, age.elapsed_for(&first, started));
        assert_eq!(
            Duration::from_millis(1_250),
            age.elapsed_for(&first, started + Duration::from_millis(1_250))
        );
        assert_eq!(
            Duration::from_millis(2_100),
            age.elapsed_for(&first, started + Duration::from_millis(2_100)),
            "a retry of the pending byte chunk must not restart its age"
        );
        assert_eq!(
            Duration::ZERO,
            age.elapsed_for(&next, started + Duration::from_millis(2_200)),
            "a new chunk starts a fresh quantum after the prior chunk progressed"
        );
    }
}
