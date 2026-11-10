use std::ops::Range;

use crate::{
    hls::{HlsMediaSegment, mp4_initialization_length},
    mp4_segments::{Mp4SidxIndex, mp4_sidx_index, mp4_sidx_segment_ranges},
};

pub(crate) const HLS_READINESS_BUDGET_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const HLS_READINESS_MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const HLS_READINESS_MAX_RANGES: usize = 128;
pub(crate) const HLS_READINESS_MAX_SIDX_PREFIX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PlaybackWindow {
    pub(crate) start_seconds: f64,
    pub(crate) duration_seconds: f64,
}

impl PlaybackWindow {
    pub(crate) fn new(start_seconds: f64, duration_seconds: f64) -> Option<Self> {
        let end_seconds = start_seconds + duration_seconds;
        (start_seconds.is_finite()
            && start_seconds >= 0.0
            && duration_seconds.is_finite()
            && duration_seconds > 0.0
            && duration_seconds <= 60.0
            && end_seconds.is_finite()
            && end_seconds > start_seconds)
            .then_some(Self {
                start_seconds,
                duration_seconds,
            })
    }

    pub(crate) fn end_seconds(self) -> f64 {
        self.start_seconds + self.duration_seconds
    }

    pub(crate) fn is_valid(self) -> bool {
        Self::new(self.start_seconds, self.duration_seconds).is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheWindowState {
    Readable,
    Missing,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheWindowReason {
    None,
    NotCached,
    IndexUnavailable,
    OutOfRange,
    Busy,
    BudgetExceeded,
    IdentityChanged,
    IoFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CacheWindowInspection {
    pub(crate) state: CacheWindowState,
    pub(crate) reason: CacheWindowReason,
    pub(crate) checked_bytes: u64,
}

impl CacheWindowInspection {
    pub(crate) const fn readable(checked_bytes: u64) -> Self {
        Self {
            state: CacheWindowState::Readable,
            reason: CacheWindowReason::None,
            checked_bytes,
        }
    }

    pub(crate) const fn missing(reason: CacheWindowReason, checked_bytes: u64) -> Self {
        Self {
            state: CacheWindowState::Missing,
            reason,
            checked_bytes,
        }
    }

    pub(crate) const fn unknown(reason: CacheWindowReason, checked_bytes: u64) -> Self {
        Self {
            state: CacheWindowState::Unknown,
            reason,
            checked_bytes,
        }
    }
}

pub(crate) struct SidxPrefixProbe {
    pub(crate) initialization_length: u64,
    pub(crate) trailing_offset: u64,
    pub(crate) trailing_length: u64,
    index: Mp4SidxIndex,
}

pub(crate) fn sidx_prefix_probe(prefix: &[u8], total_length: u64) -> Option<SidxPrefixProbe> {
    if prefix.len() > HLS_READINESS_MAX_SIDX_PREFIX_BYTES as usize {
        return None;
    }
    let initialization_length = mp4_initialization_length(prefix)?;
    let index = mp4_sidx_index(prefix, initialization_length, total_length)?;
    Some(SidxPrefixProbe {
        initialization_length,
        trailing_offset: index.trailing_offset,
        trailing_length: index.trailing_length,
        index,
    })
}

impl SidxPrefixProbe {
    pub(crate) fn into_segments(self, trailer: &[u8]) -> Option<Vec<HlsMediaSegment>> {
        mp4_sidx_segment_ranges(self.index, trailer).map(|ranges| {
            ranges
                .into_iter()
                .map(|range| HlsMediaSegment {
                    byte_range_offset: range.offset,
                    byte_range_length: range.length,
                    duration_millis: range.duration_millis,
                })
                .collect()
        })
    }
}

pub(crate) fn selected_playback_ranges(
    initialization_length: u64,
    total_length: u64,
    segments: &[HlsMediaSegment],
    window: PlaybackWindow,
) -> Result<Vec<Range<u64>>, CacheWindowReason> {
    if !window.is_valid()
        || initialization_length == 0
        || initialization_length >= total_length
        || segments.is_empty()
    {
        return Err(CacheWindowReason::IndexUnavailable);
    }

    let mut byte_cursor = initialization_length;
    let mut timeline_millis = 0_u64;
    let mut selected = Vec::with_capacity(segments.len() + 1);
    selected.push(0..initialization_length);
    for segment in segments {
        if segment.byte_range_offset != byte_cursor
            || segment.byte_range_length == 0
            || segment.duration_millis == 0
        {
            return Err(CacheWindowReason::IndexUnavailable);
        }
        let byte_end = segment
            .byte_range_offset
            .checked_add(segment.byte_range_length)
            .ok_or(CacheWindowReason::IndexUnavailable)?;
        let next_timeline_millis = timeline_millis
            .checked_add(segment.duration_millis)
            .ok_or(CacheWindowReason::IndexUnavailable)?;
        let segment_start_seconds = timeline_millis as f64 / 1_000.0;
        let segment_end_seconds = next_timeline_millis as f64 / 1_000.0;
        if segment_start_seconds < window.end_seconds()
            && window.start_seconds < segment_end_seconds
        {
            selected.push(segment.byte_range_offset..byte_end);
        }
        byte_cursor = byte_end;
        timeline_millis = next_timeline_millis;
        if selected.len() > HLS_READINESS_MAX_RANGES {
            return Err(CacheWindowReason::BudgetExceeded);
        }
    }
    if byte_cursor != total_length {
        return Err(CacheWindowReason::IndexUnavailable);
    }
    if window.end_seconds() > timeline_millis as f64 / 1_000.0 {
        return Err(CacheWindowReason::OutOfRange);
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::{
        CacheWindowReason, HLS_READINESS_MAX_RANGES, PlaybackWindow, selected_playback_ranges,
    };
    use crate::hls::HlsMediaSegment;

    #[test]
    fn playback_window_rejects_invalid_and_overflowing_values() {
        for (start, duration) in [
            (-1.0, 1.0),
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
            (0.0, 0.0),
            (0.0, 60.001),
            (f64::MAX, 1.0),
        ] {
            assert!(PlaybackWindow::new(start, duration).is_none());
        }
    }

    #[test]
    fn selects_initialization_and_only_overlapping_fragments() {
        let segments = [
            HlsMediaSegment {
                byte_range_offset: 10,
                byte_range_length: 20,
                duration_millis: 1_000,
            },
            HlsMediaSegment {
                byte_range_offset: 30,
                byte_range_length: 30,
                duration_millis: 2_000,
            },
        ];
        assert_eq!(
            Ok(vec![0..10, 10..30, 30..60]),
            selected_playback_ranges(
                10,
                60,
                &segments,
                PlaybackWindow::new(0.5, 1.0).expect("valid window"),
            )
        );
        assert_eq!(
            Ok(vec![0..10, 30..60]),
            selected_playback_ranges(
                10,
                60,
                &segments,
                PlaybackWindow::new(1.0, 1.0).expect("valid window"),
            )
        );
    }

    #[test]
    fn rejects_out_of_range_and_unbounded_range_selection() {
        let one = [HlsMediaSegment {
            byte_range_offset: 1,
            byte_range_length: 1,
            duration_millis: 1_000,
        }];
        assert_eq!(
            Err(CacheWindowReason::OutOfRange),
            selected_playback_ranges(
                1,
                2,
                &one,
                PlaybackWindow::new(0.5, 1.0).expect("valid window"),
            )
        );

        let segments = (0..=HLS_READINESS_MAX_RANGES)
            .map(|index| HlsMediaSegment {
                byte_range_offset: index as u64 + 1,
                byte_range_length: 1,
                duration_millis: 468,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            Err(CacheWindowReason::BudgetExceeded),
            selected_playback_ranges(
                1,
                HLS_READINESS_MAX_RANGES as u64 + 2,
                &segments,
                PlaybackWindow::new(0.0, 60.0).expect("valid window"),
            )
        );
    }
}
