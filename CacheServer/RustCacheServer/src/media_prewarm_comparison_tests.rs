use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::mp4_segments::Mp4SegmentRange;

pub(super) const PREWARM_BYTE_BUDGET: u64 = 8 * 1024 * 1024;
pub(super) const PREWARM_TRANSFER_BYTE_BUDGET: u64 = 16 * 1024 * 1024;
const PREWARM_TRANSFER_RANGE_LIMIT: usize = 128;
const SAFETY_SECONDS: f64 = 20.0;
const WINDOW_SECONDS: f64 = 20.0;
const SNAPSHOT_MAX_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PrewarmMode {
    Off,
    On,
}

impl PrewarmMode {
    pub(super) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            Some("off") => Ok(Self::Off),
            Some("on") => Ok(Self::On),
            _ => Err("LAN_MEDIA_ACCEPTANCE_PREWARM must explicitly be off or on".to_owned()),
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct RecoverySnapshot {
    pub schema_version: u32,
    pub observed_monotonic_seconds: f64,
    pub playhead_seconds: f64,
    pub loaded_end_seconds: f64,
}

#[derive(Deserialize)]
struct SnapshotJson {
    schema_version: u32,
    observed_monotonic_seconds: f64,
    playhead_seconds: f64,
    loaded_end_seconds: f64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SelectedRange {
    pub resource_id: String,
    pub start: u64,
    pub end: u64,
}

impl SelectedRange {
    pub fn byte_len(&self) -> Result<u64, String> {
        self.end
            .checked_sub(self.start)
            .filter(|length| *length > 0)
            .ok_or_else(|| "selected range was empty or inverted".to_owned())
    }
}

#[derive(Clone, Debug)]
pub(super) struct PrewarmPlan {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub planned_bytes: u64,
    pub ranges: Vec<SelectedRange>,
    pub resource_ids: PrewarmResourceIds,
    pub transfer_ranges: Vec<SelectedRange>,
    pub planned_transfer_bytes: u64,
    pub video_total_length: u64,
    pub audio_total_length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct PrewarmResourceIds {
    pub video: String,
    pub audio: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PrewarmComparisonReport {
    pub schema_version: u32,
    pub mode: &'static str,
    pub boundary_policy: &'static str,
    pub recovery_snapshot: RecoverySnapshot,
    pub target_window_start_seconds: f64,
    pub target_window_end_seconds: f64,
    pub byte_budget: u64,
    pub planned_bytes: u64,
    pub transfer_byte_budget: u64,
    pub planned_transfer_bytes: u64,
    pub operation_requested_bytes: u64,
    pub operation_count: u64,
    pub attempted_ranges: Vec<PrewarmAttempt>,
    pub completed_transfer_bytes: u64,
    pub completed_transfer_count: u64,
    pub preempted_operation_count: u64,
    pub foreground_defer_count: u64,
    pub elapsed_seconds: f64,
    pub coverage_semantics: &'static str,
    pub cache_covered_bytes_before: u64,
    pub cache_covered_bytes_after: Option<u64>,
    pub selected_ranges: Vec<SelectedRange>,
    pub transfer_plan: Vec<SelectedRange>,
    pub resource_ids: PrewarmResourceIds,
    pub completed: bool,
    pub failure: PrewarmFailure,
    pub provenance: PrewarmProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PrewarmOutcome {
    InProgress,
    Completed,
    Preempted,
    Cancelled,
    SessionRemoving,
    IdentityChanged,
    QuotaExceeded,
    UpstreamStatus,
    Network,
    Io,
    RangeUnsupported,
    InvalidResponse,
    DeadlineExceeded,
    ResourceLengthChanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PrewarmFailureCategory {
    PlanInvalid,
    Cancelled,
    SessionRemoving,
    IdentityChanged,
    QuotaExceeded,
    UpstreamStatus,
    Network,
    Io,
    RangeUnsupported,
    InvalidResponse,
    DeadlineExceeded,
    AttemptLimit,
    ByteBudgetExceeded,
    ResourceLengthChanged,
    CacheCoverageIncomplete,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct PrewarmAttempt {
    pub resource_id: String,
    pub start: u64,
    pub end: u64,
    pub outcome: PrewarmOutcome,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct PrewarmFailure {
    pub category: Option<PrewarmFailureCategory>,
    pub resource_id: Option<String>,
    pub start: Option<u64>,
    pub end: Option<u64>,
    pub upstream_status: Option<u16>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PrewarmProvenance {
    pub origin_counters_include_playback_and_prewarm: bool,
    pub transfer_accounting: &'static str,
    pub expanded_transfer_may_overlap_buffered_media: bool,
}

pub(super) struct PrewarmTrackInput<'a> {
    pub track_index: usize,
    pub resource_id: &'a str,
    pub actual_total_length: u64,
    pub initialization_bytes: u64,
    pub fragments: &'a [Mp4SegmentRange],
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PrewarmExecution {
    pub operation_requested_bytes: u64,
    pub operation_count: u64,
    pub attempted_ranges: Vec<PrewarmAttempt>,
    pub completed_transfer_bytes: u64,
    pub completed_transfer_count: u64,
    pub preempted_operation_count: u64,
    pub foreground_defer_count: u64,
    pub elapsed_seconds: f64,
    pub coverage_before: u64,
    pub coverage_after: Option<u64>,
    pub completed: bool,
    pub failure: PrewarmFailure,
}

impl PrewarmExecution {
    pub(super) fn initial(coverage: u64) -> Self {
        Self {
            operation_requested_bytes: 0,
            operation_count: 0,
            attempted_ranges: Vec::new(),
            completed_transfer_bytes: 0,
            completed_transfer_count: 0,
            preempted_operation_count: 0,
            foreground_defer_count: 0,
            elapsed_seconds: 0.0,
            coverage_before: coverage,
            coverage_after: None,
            completed: false,
            failure: PrewarmFailure::default(),
        }
    }
}

pub(super) fn read_recovery_snapshot(
    path: &Path,
    recovery_start_before: f64,
    now_monotonic: f64,
    source_duration_seconds: f64,
) -> Result<RecoverySnapshot, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "recovery snapshot could not be inspected".to_owned())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > SNAPSHOT_MAX_BYTES
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err("recovery snapshot was not a bounded private regular file".to_owned());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "recovery snapshot could not be read without following links".to_owned())?;
    let opened = file
        .metadata()
        .map_err(|_| "opened recovery snapshot metadata could not be read".to_owned())?;
    if !opened.is_file()
        || opened.len() != metadata.len()
        || opened.permissions().mode() & 0o777 != 0o600
    {
        return Err("recovery snapshot changed while being opened".to_owned());
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take(SNAPSHOT_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "recovery snapshot could not be read".to_owned())?;
    if bytes.len() as u64 != metadata.len() || bytes.len() as u64 > SNAPSHOT_MAX_BYTES {
        return Err("recovery snapshot changed size while being read".to_owned());
    }
    let json: SnapshotJson = serde_json::from_slice(&bytes)
        .map_err(|_| "recovery snapshot JSON was invalid".to_owned())?;
    let snapshot = RecoverySnapshot {
        schema_version: json.schema_version,
        observed_monotonic_seconds: json.observed_monotonic_seconds,
        playhead_seconds: json.playhead_seconds,
        loaded_end_seconds: json.loaded_end_seconds,
    };
    if snapshot.schema_version != 1
        || !snapshot.observed_monotonic_seconds.is_finite()
        || !snapshot.playhead_seconds.is_finite()
        || !snapshot.loaded_end_seconds.is_finite()
        || !source_duration_seconds.is_finite()
        || snapshot.observed_monotonic_seconds < recovery_start_before
        || snapshot.observed_monotonic_seconds > now_monotonic
        || snapshot.playhead_seconds < 0.0
        || snapshot.playhead_seconds > snapshot.loaded_end_seconds
        || snapshot.loaded_end_seconds > source_duration_seconds + 0.25
    {
        return Err("recovery snapshot values were outside validated bounds".to_owned());
    }
    Ok(snapshot)
}

pub(super) fn plan_window(
    snapshot: &RecoverySnapshot,
    source_duration_seconds: f64,
    tracks: [PrewarmTrackInput<'_>; 2],
) -> Result<PrewarmPlan, String> {
    let start_seconds =
        (snapshot.playhead_seconds + SAFETY_SECONDS).max(snapshot.loaded_end_seconds);
    let end_seconds = (start_seconds + WINDOW_SECONDS).min(source_duration_seconds);
    if !start_seconds.is_finite()
        || !end_seconds.is_finite()
        || end_seconds <= start_seconds
        || end_seconds - start_seconds < 1.0
    {
        return Err(
            "recovery snapshot left no useful prewarm window before source tail".to_owned(),
        );
    }

    let mut ranges = Vec::new();
    let mut transfer_ranges = Vec::new();
    let mut resource_ids = PrewarmResourceIds {
        video: String::new(),
        audio: String::new(),
    };
    let mut selected_video_fragments = 0_u64;
    let mut selected_audio_fragments = 0_u64;
    let mut video_total_length = 0_u64;
    let mut audio_total_length = 0_u64;
    for track in tracks {
        if track.initialization_bytes == 0
            || track.actual_total_length == 0
            || track.initialization_bytes > track.actual_total_length
            || track.fragments.is_empty()
        {
            return Err("prewarm resource lacked initialization or media fragments".to_owned());
        }
        let resource_id = track.resource_id.to_owned();
        let resource_id_slot = if track.track_index == 0 {
            &mut resource_ids.video
        } else if track.track_index == 1 {
            &mut resource_ids.audio
        } else {
            return Err("prewarm resource had an unknown track index".to_owned());
        };
        if !resource_id_slot.is_empty() || resource_id.is_empty() {
            return Err("prewarm resource identifiers were empty or duplicated".to_owned());
        }
        *resource_id_slot = resource_id.clone();
        if track.track_index == 0 {
            video_total_length = track.actual_total_length;
        } else {
            audio_total_length = track.actual_total_length;
        }
        ranges.push(SelectedRange {
            resource_id: resource_id.clone(),
            start: 0,
            end: track.initialization_bytes,
        });
        let mut elapsed_millis = 0_u64;
        for fragment in track.fragments {
            let duration = fragment.duration_millis;
            let next_millis = elapsed_millis
                .checked_add(duration)
                .ok_or_else(|| "fragment timeline overflowed".to_owned())?;
            let fragment_start = elapsed_millis as f64 / 1000.0;
            let fragment_end = next_millis as f64 / 1000.0;
            if fragment_start >= start_seconds && fragment_start < end_seconds {
                let byte_end = fragment
                    .offset
                    .checked_add(fragment.length)
                    .ok_or_else(|| "fragment byte range overflowed".to_owned())?;
                if fragment.length == 0 {
                    return Err("selected media fragment had an empty range".to_owned());
                }
                if byte_end > track.actual_total_length {
                    return Err("selected media fragment exceeded its resource length".to_owned());
                }
                ranges.push(SelectedRange {
                    resource_id: resource_id.clone(),
                    start: fragment.offset,
                    end: byte_end,
                });
                if track.track_index == 0 {
                    selected_video_fragments = selected_video_fragments
                        .checked_add(1)
                        .ok_or_else(|| "selected video fragment count overflowed".to_owned())?;
                } else {
                    selected_audio_fragments = selected_audio_fragments
                        .checked_add(1)
                        .ok_or_else(|| "selected audio fragment count overflowed".to_owned())?;
                }
            }
            if fragment_end <= fragment_start {
                return Err("fragment had an invalid time range".to_owned());
            }
            elapsed_millis = next_millis;
        }
        let requested = ranges
            .iter()
            .filter(|range| range.resource_id == resource_id)
            .map(|range| range.start..range.end)
            .collect::<Vec<_>>();
        let parents = crate::hls_cache::planned_background_ranges_for_test(
            track.actual_total_length,
            &requested,
        )?;
        transfer_ranges.extend(parents.into_iter().map(|range| SelectedRange {
            resource_id: resource_id.clone(),
            start: range.start,
            end: range.end,
        }));
    }
    if selected_video_fragments == 0 || selected_audio_fragments == 0 {
        return Err("prewarm window lacked high video or shared audio media fragments".to_owned());
    }
    if resource_ids.video == resource_ids.audio {
        return Err("prewarm video and audio resource identifiers were not distinct".to_owned());
    }
    ranges.sort_by(|left, right| {
        (&left.resource_id, left.start, left.end).cmp(&(&right.resource_id, right.start, right.end))
    });
    if ranges
        .windows(2)
        .any(|pair| pair[0].resource_id == pair[1].resource_id && pair[0].end > pair[1].start)
    {
        return Err("selected prewarm ranges overlapped or were not canonical".to_owned());
    }
    if ranges.len() > PREWARM_TRANSFER_RANGE_LIMIT {
        return Err("selected prewarm plan exceeded the 128-range limit".to_owned());
    }
    let planned_bytes = ranges.iter().try_fold(0_u64, |total, range| {
        total
            .checked_add(range.byte_len()?)
            .ok_or_else(|| "combined prewarm byte count overflowed".to_owned())
    })?;
    if planned_bytes > PREWARM_BYTE_BUDGET {
        return Err("selected prewarm ranges exceeded the 8 MiB combined budget".to_owned());
    }
    transfer_ranges.sort_by(|left, right| {
        (&left.resource_id, left.start, left.end).cmp(&(&right.resource_id, right.start, right.end))
    });
    let mut canonical_transfer_ranges: Vec<SelectedRange> =
        Vec::with_capacity(transfer_ranges.len());
    for range in transfer_ranges {
        if let Some(previous) = canonical_transfer_ranges.last_mut()
            && previous.resource_id == range.resource_id
            && previous.end == range.start
        {
            previous.end = range.end;
        } else {
            canonical_transfer_ranges.push(range);
        }
    }
    let transfer_ranges = canonical_transfer_ranges;
    if transfer_ranges.len() > PREWARM_TRANSFER_RANGE_LIMIT {
        return Err("expanded prewarm plan exceeded the 128-range limit".to_owned());
    }
    let planned_transfer_bytes = transfer_ranges.iter().try_fold(0_u64, |total, range| {
        total
            .checked_add(range.byte_len()?)
            .ok_or_else(|| "expanded prewarm byte count overflowed".to_owned())
    })?;
    if planned_transfer_bytes > PREWARM_TRANSFER_BYTE_BUDGET {
        return Err("expanded prewarm ranges exceeded the 16 MiB transfer budget".to_owned());
    }
    for selected in &ranges {
        let contained = transfer_ranges.iter().any(|transfer| {
            transfer.resource_id == selected.resource_id
                && transfer.start <= selected.start
                && selected.end <= transfer.end
        });
        if !contained {
            return Err("expanded prewarm plan did not contain every selected range".to_owned());
        }
    }
    Ok(PrewarmPlan {
        start_seconds,
        end_seconds,
        planned_bytes,
        ranges,
        resource_ids,
        transfer_ranges,
        planned_transfer_bytes,
        video_total_length,
        audio_total_length,
    })
}

pub(super) fn report_for(
    mode: PrewarmMode,
    snapshot: RecoverySnapshot,
    plan: PrewarmPlan,
    execution: PrewarmExecution,
) -> PrewarmComparisonReport {
    PrewarmComparisonReport {
        schema_version: 3,
        mode: mode.label(),
        boundary_policy: "unbuffered_after_20s_safety",
        recovery_snapshot: snapshot,
        target_window_start_seconds: plan.start_seconds,
        target_window_end_seconds: plan.end_seconds,
        byte_budget: PREWARM_BYTE_BUDGET,
        planned_bytes: plan.planned_bytes,
        transfer_byte_budget: PREWARM_TRANSFER_BYTE_BUDGET,
        planned_transfer_bytes: plan.planned_transfer_bytes,
        operation_requested_bytes: execution.operation_requested_bytes,
        operation_count: execution.operation_count,
        attempted_ranges: execution.attempted_ranges,
        completed_transfer_bytes: execution.completed_transfer_bytes,
        completed_transfer_count: execution.completed_transfer_count,
        preempted_operation_count: execution.preempted_operation_count,
        foreground_defer_count: execution.foreground_defer_count,
        elapsed_seconds: execution.elapsed_seconds,
        coverage_semantics: "sum_of_fully_readable_selected_ranges",
        cache_covered_bytes_before: execution.coverage_before,
        cache_covered_bytes_after: execution.coverage_after,
        selected_ranges: plan.ranges,
        transfer_plan: plan.transfer_ranges,
        resource_ids: plan.resource_ids,
        completed: execution.completed,
        failure: execution.failure,
        provenance: PrewarmProvenance {
            origin_counters_include_playback_and_prewarm: true,
            transfer_accounting: "planned_background_parent_chunks_and_startup_not_wire_bytes",
            expanded_transfer_may_overlap_buffered_media: true,
        },
    }
}

async fn run_native_single_player_prewarm_control() {
    crate::media_acceptance_tests::run_single_player_prewarm_case(PrewarmMode::Off).await;
}

async fn run_native_single_player_prewarm_treatment() {
    crate::media_acceptance_tests::run_single_player_prewarm_case(PrewarmMode::On).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic only; requires explicit macOS AVPlayer probe and local FFmpeg tools"]
async fn native_single_player_prewarm_control() {
    run_native_single_player_prewarm_control().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic only; requires explicit macOS AVPlayer probe and local FFmpeg tools"]
async fn native_single_player_prewarm_treatment() {
    run_native_single_player_prewarm_treatment().await;
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use std::io::Write;

    fn fragments(count: usize, size: u64) -> Vec<Mp4SegmentRange> {
        (0..count)
            .map(|index| Mp4SegmentRange {
                offset: 100 + index as u64 * size,
                length: size,
                duration_millis: 1_000,
            })
            .collect()
    }

    fn snapshot(playhead: f64, loaded: f64) -> RecoverySnapshot {
        RecoverySnapshot {
            schema_version: 1,
            observed_monotonic_seconds: 10.0,
            playhead_seconds: playhead,
            loaded_end_seconds: loaded,
        }
    }

    fn tracks<'a>(
        video_initialization_bytes: u64,
        video_fragments: &'a [Mp4SegmentRange],
        audio_initialization_bytes: u64,
        audio_fragments: &'a [Mp4SegmentRange],
    ) -> [PrewarmTrackInput<'a>; 2] {
        [
            PrewarmTrackInput {
                track_index: 0,
                resource_id: "high",
                actual_total_length: 2 * 1024 * 1024,
                initialization_bytes: video_initialization_bytes,
                fragments: video_fragments,
            },
            PrewarmTrackInput {
                track_index: 1,
                resource_id: "audio",
                actual_total_length: 2 * 1024 * 1024,
                initialization_bytes: audio_initialization_bytes,
                fragments: audio_fragments,
            },
        ]
    }

    fn completed_execution(
        plan: &PrewarmPlan,
        coverage_before: u64,
        coverage_after: u64,
    ) -> PrewarmExecution {
        let mut execution = PrewarmExecution::initial(coverage_before);
        execution.operation_requested_bytes = plan.planned_transfer_bytes;
        execution.operation_count = plan.transfer_ranges.len() as u64;
        execution.attempted_ranges = plan
            .transfer_ranges
            .iter()
            .map(|range| PrewarmAttempt {
                resource_id: range.resource_id.clone(),
                start: range.start,
                end: range.end,
                outcome: PrewarmOutcome::Completed,
            })
            .collect();
        execution.completed_transfer_bytes = plan.planned_transfer_bytes;
        execution.completed_transfer_count = plan.transfer_ranges.len() as u64;
        execution.elapsed_seconds = 1.25;
        execution.coverage_after = Some(coverage_after);
        execution.completed = true;
        execution
    }

    #[test]
    fn mode_requires_explicit_known_value() {
        assert_eq!(PrewarmMode::parse(Some("off")).unwrap(), PrewarmMode::Off);
        assert_eq!(PrewarmMode::parse(Some("on")).unwrap(), PrewarmMode::On);
        assert!(PrewarmMode::parse(None).is_err());
        assert!(PrewarmMode::parse(Some("maybe")).is_err());
    }

    #[test]
    fn planner_selects_complete_video_and_audio_fragments_after_boundary() {
        let plan = plan_window(
            &snapshot(40.0, 48.0),
            3600.0,
            tracks(64, &fragments(100, 200), 32, &fragments(100, 100)),
        )
        .unwrap();
        assert_eq!(plan.start_seconds, 60.0);
        assert_eq!(plan.end_seconds, 80.0);
        assert_eq!(plan.ranges.len(), 42);
        assert!(
            plan.ranges
                .iter()
                .any(|range| range.resource_id == "high" && range.start == 12_100)
        );
        assert!(
            plan.ranges
                .iter()
                .any(|range| range.resource_id == "audio" && range.start == 6_100)
        );
    }

    #[test]
    fn planner_rejects_tail_empty_and_budget_overflow() {
        assert!(
            plan_window(
                &snapshot(3580.0, 3590.0),
                3600.0,
                tracks(1, &fragments(3600, 4), 1, &fragments(3600, 4)),
            )
            .is_err()
        );
        assert!(plan_window(&snapshot(40.0, 48.0), 3600.0, tracks(1, &[], 1, &[]),).is_err());
        assert!(
            plan_window(
                &snapshot(40.0, 48.0),
                3600.0,
                tracks(1, &fragments(100, 300_000), 1, &fragments(100, 300_000)),
            )
            .is_err()
        );
        let mut overflow = fragments(100, 4);
        overflow[60].offset = u64::MAX;
        assert!(
            plan_window(
                &snapshot(40.0, 48.0),
                3600.0,
                tracks(1, &overflow, 1, &fragments(100, 4)),
            )
            .is_err()
        );
        assert!(
            SelectedRange {
                resource_id: "x".to_owned(),
                start: 8,
                end: 7
            }
            .byte_len()
            .is_err()
        );
    }

    #[test]
    fn snapshot_checks_file_policy_schema_and_monotonic_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("snapshot.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"schema_version":1,"observed_monotonic_seconds":10.5,"playhead_seconds":42.0,"loaded_end_seconds":48.0}"#).unwrap();
        drop(file);
        assert!(read_recovery_snapshot(&path, 10.0, 11.0, 3600.0).is_ok());
        assert!(read_recovery_snapshot(&path, 10.6, 11.0, 3600.0).is_err());
        assert!(read_recovery_snapshot(&path, 10.0, 10.4, 3600.0).is_err());
        assert!(read_recovery_snapshot(&path, 10.0, 11.0, 40.0).is_err());
        let link = directory.path().join("snapshot-link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_recovery_snapshot(&link, 10.0, 11.0, 3600.0).is_err());
    }

    #[test]
    fn report_accounting_distinguishes_control_from_treatment() {
        let plan = plan_window(
            &snapshot(40.0, 48.0),
            3600.0,
            tracks(64, &fragments(100, 20), 32, &fragments(100, 10)),
        )
        .unwrap();
        let control = report_for(
            PrewarmMode::Off,
            snapshot(40.0, 48.0),
            plan.clone(),
            PrewarmExecution {
                elapsed_seconds: 0.0,
                completed: true,
                ..PrewarmExecution::initial(4)
            },
        );
        let treatment = report_for(
            PrewarmMode::On,
            snapshot(40.0, 48.0),
            plan.clone(),
            completed_execution(&plan, 4, 100),
        );
        assert_eq!(control.operation_requested_bytes, 0);
        assert_eq!(control.operation_count, 0);
        assert_eq!(control.schema_version, 3);
        assert!(control.attempted_ranges.is_empty());
        assert_eq!(control.completed_transfer_bytes, 0);
        assert_eq!(control.completed_transfer_count, 0);
        assert_eq!(control.preempted_operation_count, 0);
        assert_eq!(control.foreground_defer_count, 0);
        assert!(control.failure.category.is_none());
        assert!(control.failure.resource_id.is_none());
        assert!(control.failure.start.is_none());
        assert!(control.failure.end.is_none());
        assert!(control.failure.upstream_status.is_none());
        assert_eq!(control.transfer_byte_budget, PREWARM_TRANSFER_BYTE_BUDGET);
        assert_eq!(
            treatment.planned_transfer_bytes,
            plan.planned_transfer_bytes
        );
        assert_eq!(
            treatment.transfer_plan.len() as u64,
            treatment.operation_count
        );
        assert_eq!(
            treatment.operation_requested_bytes,
            treatment.planned_transfer_bytes
        );
        assert_eq!(
            treatment.completed_transfer_bytes,
            treatment.planned_transfer_bytes
        );
        assert_eq!(
            treatment.completed_transfer_count,
            treatment.transfer_plan.len() as u64
        );
        assert_eq!(
            treatment.operation_count,
            treatment.attempted_ranges.len() as u64
        );
        assert_eq!(treatment.failure.category, None);
        assert_eq!(
            treatment.coverage_semantics,
            "sum_of_fully_readable_selected_ranges"
        );
        assert!(
            treatment
                .provenance
                .origin_counters_include_playback_and_prewarm
        );
        assert_eq!(
            treatment.provenance.transfer_accounting,
            "planned_background_parent_chunks_and_startup_not_wire_bytes"
        );
        assert!(
            treatment
                .provenance
                .expanded_transfer_may_overlap_buffered_media
        );
        assert!(
            !report_for(
                PrewarmMode::On,
                snapshot(40.0, 48.0),
                plan,
                PrewarmExecution {
                    completed: false,
                    ..PrewarmExecution::initial(0)
                }
            )
            .completed
        );
    }

    #[test]
    fn coverage_after_serializes_unknown_as_null_and_observed_as_integer() {
        let plan = plan_window(
            &snapshot(40.0, 48.0),
            3600.0,
            tracks(64, &fragments(100, 20), 32, &fragments(100, 10)),
        )
        .unwrap();
        let mut execution = PrewarmExecution::initial(4);
        let unknown = report_for(
            PrewarmMode::On,
            snapshot(40.0, 48.0),
            plan.clone(),
            execution.clone(),
        );
        assert_eq!(
            serde_json::to_value(unknown).unwrap()["cache_covered_bytes_after"],
            serde_json::Value::Null
        );

        execution.coverage_after = Some(100);
        let observed = report_for(PrewarmMode::On, snapshot(40.0, 48.0), plan, execution);
        assert_eq!(
            serde_json::to_value(observed).unwrap()["cache_covered_bytes_after"],
            serde_json::json!(100)
        );
    }

    #[test]
    fn background_transfer_expansion_uses_real_parent_chunks_and_deduplicates() {
        let plan = crate::hls_cache::planned_background_ranges_for_test(
            1_300_000,
            &[700_000..710_000, 700_000..710_000, 1_200_000..1_250_000],
        )
        .unwrap();
        assert_eq!(
            plan,
            vec![0..524_288, 524_288..1_048_576, 1_048_576..1_300_000]
        );
        assert_eq!(
            plan.iter()
                .map(|range| range.end - range.start)
                .sum::<u64>(),
            1_300_000
        );
        assert!(crate::hls_cache::planned_background_ranges_for_test(0, &[]).is_err());
        let overflow = std::ops::Range {
            start: u64::MAX - 1,
            end: u64::MAX,
        };
        let inverted = std::ops::Range { start: 9, end: 8 };
        assert!(
            crate::hls_cache::planned_background_ranges_for_test(
                100,
                std::slice::from_ref(&overflow)
            )
            .is_err()
        );
        assert!(
            crate::hls_cache::planned_background_ranges_for_test(
                100,
                std::slice::from_ref(&inverted)
            )
            .is_err()
        );
    }

    #[test]
    fn planner_rejects_expanded_transfer_above_budget_before_execution() {
        let mut video = fragments(100, 1);
        let mut audio = fragments(100, 1);
        for (index, fragment) in video.iter_mut().enumerate().skip(60).take(20) {
            fragment.offset = (index as u64) * 128 * 1024 * 1024;
        }
        for (index, fragment) in audio.iter_mut().enumerate().skip(60).take(20) {
            fragment.offset = (index as u64) * 128 * 1024 * 1024;
        }
        let plan = plan_window(
            &snapshot(40.0, 48.0),
            3600.0,
            [
                PrewarmTrackInput {
                    track_index: 0,
                    resource_id: "high",
                    actual_total_length: 64 * 1024 * 1024 * 1024,
                    initialization_bytes: 64,
                    fragments: &video,
                },
                PrewarmTrackInput {
                    track_index: 1,
                    resource_id: "audio",
                    actual_total_length: 64 * 1024 * 1024 * 1024,
                    initialization_bytes: 32,
                    fragments: &audio,
                },
            ],
        );
        assert!(plan.is_err());
    }
}
