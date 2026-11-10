use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_TIMING_BOX_PAYLOAD_LENGTH: u64 = 16 * 1024 * 1024;
const MAX_SIDX_PREFIX_BYTES: usize = 1024 * 1024;
const MAX_SIDX_REFERENCES: usize = 16_384;
const MAX_HLS_BANDWIDTH_RANGES: usize = MAX_SIDX_REFERENCES;
const MAX_HLS_BANDWIDTH_CANDIDATES: usize = 1_000_000;

pub(crate) const MAX_SIDX_TRAILER_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Mp4SegmentRange {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) duration_millis: u64,
}

pub(crate) fn hls_target_duration_for_ranges(ranges: &[Mp4SegmentRange]) -> Option<u64> {
    if ranges.is_empty() || ranges.len() > MAX_HLS_BANDWIDTH_RANGES {
        return None;
    }
    ranges
        .iter()
        .try_fold(0_u64, |target, range| {
            if range.duration_millis == 0 {
                return None;
            }
            let seconds = range.duration_millis / 1_000;
            let remainder = range.duration_millis % 1_000;
            Some(target.max((seconds + u64::from(remainder >= 500)).max(1)))
        })
        .filter(|target| *target > 0)
}

pub(crate) fn hls_peak_bandwidth_for_ranges(
    ranges: &[Mp4SegmentRange],
    target_seconds: u64,
) -> Option<u64> {
    if ranges.is_empty() || ranges.len() > MAX_HLS_BANDWIDTH_RANGES || target_seconds == 0 {
        return None;
    }
    let target_millis = target_seconds.checked_mul(1_000)?;
    let minimum_window_twice = target_millis;
    let maximum_window_twice = target_millis.checked_mul(3)?;
    let mut peak = 0_u64;
    let mut candidates = 0_usize;
    for start in 0..ranges.len() {
        let mut bytes = 0_u64;
        let mut duration_millis = 0_u64;
        for range in &ranges[start..] {
            candidates = candidates.checked_add(1)?;
            if candidates > MAX_HLS_BANDWIDTH_CANDIDATES {
                return None;
            }
            if range.length == 0 || range.duration_millis == 0 {
                return None;
            }
            bytes = bytes.checked_add(range.length)?;
            duration_millis = duration_millis.checked_add(range.duration_millis)?;
            let duration_twice = duration_millis.checked_mul(2)?;
            if duration_twice > maximum_window_twice {
                break;
            }
            if duration_twice >= minimum_window_twice {
                let numerator = bytes.checked_mul(8)?.checked_mul(1_000)?;
                let rounded =
                    numerator.checked_add(duration_millis.checked_sub(1)?)? / duration_millis;
                peak = peak.max(rounded);
            }
        }
    }
    (peak > 0).then_some(peak)
}

pub(crate) struct Mp4SidxIndex {
    pub(crate) trailing_offset: u64,
    pub(crate) trailing_length: u64,
    ranges: Vec<Mp4SegmentRange>,
    first_reference_offset: u64,
    total_length: u64,
}

pub(crate) fn mp4_sidx_index(
    prefix: &[u8],
    initialization_length: u64,
    total_length: u64,
) -> Option<Mp4SidxIndex> {
    if prefix.len() > MAX_SIDX_PREFIX_BYTES
        || total_length <= initialization_length
        || initialization_length > u64::try_from(prefix.len()).ok()?
    {
        return None;
    }

    let initialization_length = usize::try_from(initialization_length).ok()?;
    let mut offset = 0_usize;
    while offset < initialization_length {
        let (kind, _, _, next_offset) = read_slice_box(prefix, offset)?;
        if kind == *b"sidx" || next_offset > initialization_length {
            return None;
        }
        offset = next_offset;
    }
    if offset != initialization_length {
        return None;
    }

    let mut offset = initialization_length;
    let mut parsed = None;
    while offset < prefix.len() {
        let (kind, payload_start, payload_end, next_offset) = read_slice_box(prefix, offset)?;
        if kind == *b"sidx" {
            if parsed.is_some() {
                return None;
            }
            parsed = Some(parse_sidx(
                &prefix[payload_start..payload_end],
                u64::try_from(offset).ok()?,
                u64::try_from(next_offset).ok()?,
                initialization_length as u64,
                total_length,
            )?);
        } else if matches!(&kind, b"moof" | b"mdat") && parsed.is_none() {
            return None;
        }
        if let Some(index) = parsed.as_ref()
            && u64::try_from(next_offset).ok()? > index.first_reference_offset
        {
            return None;
        }
        offset = next_offset;
        if let Some(index) = parsed.as_ref()
            && u64::try_from(offset).ok()? >= index.first_reference_offset
        {
            break;
        }
    }

    let mut index = parsed?;
    let references_end = index
        .ranges
        .last()?
        .offset
        .checked_add(index.ranges.last()?.length)?;
    if references_end > total_length {
        return None;
    }
    let trailing_length = total_length.checked_sub(references_end)?;
    if trailing_length > MAX_SIDX_TRAILER_BYTES {
        return None;
    }
    index.trailing_offset = references_end;
    index.trailing_length = trailing_length;
    Some(index)
}

pub(crate) fn mp4_sidx_segment_ranges(
    index: Mp4SidxIndex,
    trailer: &[u8],
) -> Option<Vec<Mp4SegmentRange>> {
    if u64::try_from(trailer.len()).ok()? != index.trailing_length {
        return None;
    }
    if index.trailing_length == 0 {
        if !trailer.is_empty()
            || index
                .ranges
                .last()?
                .offset
                .checked_add(index.ranges.last()?.length)?
                != index.total_length
        {
            return None;
        }
        return Some(index.ranges);
    }

    if !is_valid_terminal_mfra_bytes(trailer)? {
        return None;
    }
    let mut ranges = index.ranges;
    let last = ranges.last_mut()?;
    let end = last.offset.checked_add(last.length)?;
    if end != index.trailing_offset {
        return None;
    }
    last.length = index.total_length.checked_sub(last.offset)?;
    Some(ranges)
}

fn parse_sidx(
    payload: &[u8],
    box_offset: u64,
    box_end: u64,
    initialization_length: u64,
    total_length: u64,
) -> Option<Mp4SidxIndex> {
    let version = *payload.first()?;
    if !matches!(version, 0 | 1) || full_box_flags(payload)? != 0 {
        return None;
    }
    let timescale = u64::from(read_u32(payload, 8)?);
    if timescale == 0 {
        return None;
    }
    let (earliest, first_offset, mut cursor) = if version == 0 {
        (
            u64::from(read_u32(payload, 12)?),
            u64::from(read_u32(payload, 16)?),
            20_usize,
        )
    } else {
        (read_u64(payload, 12)?, read_u64(payload, 20)?, 28_usize)
    };
    if payload.get(cursor..cursor.checked_add(2)?)? != [0, 0] {
        return None;
    }
    cursor = cursor.checked_add(2)?;
    let reference_count = usize::from(read_u16(payload, cursor)?);
    cursor = cursor.checked_add(2)?;
    if !(2..=MAX_SIDX_REFERENCES).contains(&reference_count)
        || cursor.checked_add(reference_count.checked_mul(12)?)? != payload.len()
    {
        return None;
    }

    let first_reference_offset = box_end.checked_add(first_offset)?;
    if first_reference_offset < initialization_length || first_reference_offset > total_length {
        return None;
    }
    let mut reference_offset = first_reference_offset;
    let mut cumulative_duration = 0_u64;
    let mut ranges = Vec::with_capacity(reference_count);
    for _ in 0..reference_count {
        let size_and_type = read_u32(payload, cursor)?;
        cursor = cursor.checked_add(4)?;
        let reference_size = u64::from(size_and_type & 0x7fff_ffff);
        let duration = u64::from(read_u32(payload, cursor)?);
        cursor = cursor.checked_add(4)?;
        let sap = read_u32(payload, cursor)?;
        cursor = cursor.checked_add(4)?;
        let sap_type = (sap >> 28) & 0x07;
        if size_and_type & 0x8000_0000 != 0
            || reference_size == 0
            || duration == 0
            || sap & 0x8000_0000 == 0
            || sap_type > 2
            || sap & 0x0fff_ffff != 0
        {
            return None;
        }
        cumulative_duration = cumulative_duration.checked_add(duration)?;
        let duration_millis = duration
            .checked_mul(1_000)?
            .checked_add(timescale.checked_sub(1)?)?
            .checked_div(timescale)?;
        if duration_millis == 0 {
            return None;
        }
        let reference_end = reference_offset.checked_add(reference_size)?;
        if reference_end > total_length {
            return None;
        }
        let segment_offset = if ranges.is_empty() {
            initialization_length
        } else {
            reference_offset
        };
        let segment_end = reference_end;
        if segment_end <= segment_offset {
            return None;
        }
        ranges.push(Mp4SegmentRange {
            offset: segment_offset,
            length: segment_end.checked_sub(segment_offset)?,
            duration_millis,
        });
        reference_offset = reference_end;
    }
    earliest.checked_add(cumulative_duration)?;
    if reference_offset > total_length || box_offset >= box_end {
        return None;
    }
    Some(Mp4SidxIndex {
        trailing_offset: reference_offset,
        trailing_length: 0,
        ranges,
        first_reference_offset,
        total_length,
    })
}

#[derive(Clone, Copy)]
struct Mp4BoxHeader {
    offset: u64,
    header_length: u64,
    size: u64,
    kind: [u8; 4],
}

impl Mp4BoxHeader {
    fn end(self) -> Option<u64> {
        self.offset.checked_add(self.size)
    }

    fn payload_offset(self) -> Option<u64> {
        self.offset.checked_add(self.header_length)
    }

    fn payload_length(self) -> Option<u64> {
        self.size.checked_sub(self.header_length)
    }
}

#[derive(Default)]
struct Mp4TimingContext {
    timescale_by_track: HashMap<u32, u32>,
    default_sample_duration_by_track: HashMap<u32, u32>,
}

pub(crate) fn mp4_fragment_ranges(
    path: &Path,
    initialization_length: u64,
    total_length: u64,
) -> io::Result<Vec<Mp4SegmentRange>> {
    if initialization_length >= total_length {
        return Ok(Vec::new());
    }

    let mut file = File::open(path)?;
    Ok(
        parse_mp4_fragment_ranges(&mut file, initialization_length, total_length)?
            .unwrap_or_default(),
    )
}

fn parse_mp4_fragment_ranges(
    file: &mut File,
    initialization_length: u64,
    total_length: u64,
) -> io::Result<Option<Vec<Mp4SegmentRange>>> {
    let Some(timing) = parse_timing_context(file, initialization_length, total_length)? else {
        return Ok(Some(Vec::new()));
    };
    let mut offset = initialization_length;
    let mut pending_segment_start = None;
    let mut segments = Vec::new();

    while offset < total_length {
        let Some(header) = read_box_header(file, offset, total_length)? else {
            return Ok(None);
        };

        match &header.kind {
            b"moof" => {
                let segment_start = pending_segment_start.unwrap_or(header.offset);
                let Some(fragment_body_start) = header.end() else {
                    return Ok(None);
                };
                let Some(duration_millis) = parse_moof_duration_millis(file, header, &timing)?
                else {
                    return Ok(Some(Vec::new()));
                };
                let Some(segment_end) = find_fragment_end(file, fragment_body_start, total_length)?
                else {
                    return Ok(None);
                };
                if segment_end <= segment_start {
                    return Ok(None);
                }
                segments.push(Mp4SegmentRange {
                    offset: segment_start,
                    length: segment_end - segment_start,
                    duration_millis,
                });
                offset = segment_end;
                pending_segment_start = None;
            }
            b"mdat" => return Ok(None),
            b"mfra" => {
                if pending_segment_start.is_some()
                    || segments.len() < 2
                    || header.end() != Some(total_length)
                    || !is_valid_terminal_mfra(file, header)?
                {
                    return Ok(Some(Vec::new()));
                }
                let Some(last_segment) = segments.last_mut() else {
                    return Ok(Some(Vec::new()));
                };
                if last_segment.offset.checked_add(last_segment.length) != Some(header.offset) {
                    return Ok(Some(Vec::new()));
                }
                last_segment.length = total_length - last_segment.offset;
                return Ok(Some(segments));
            }
            _ => {
                pending_segment_start.get_or_insert(header.offset);
                let Some(next_offset) = header.end() else {
                    return Ok(None);
                };
                offset = next_offset;
            }
        }
    }

    if pending_segment_start.is_some() {
        return Ok(Some(Vec::new()));
    }
    if segments.len() < 2 {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(segments))
}

fn is_valid_terminal_mfra(file: &mut File, header: Mp4BoxHeader) -> io::Result<bool> {
    let Some(payload) = read_box_payload_limited(file, header, MAX_TIMING_BOX_PAYLOAD_LENGTH)?
    else {
        return Ok(false);
    };
    let Some(mfra_size) = u32::try_from(header.size).ok() else {
        return Ok(false);
    };
    Ok(is_valid_terminal_mfra_payload(&payload, mfra_size))
}

fn is_valid_terminal_mfra_bytes(trailer: &[u8]) -> Option<bool> {
    if u64::try_from(trailer.len()).ok()? > MAX_SIDX_TRAILER_BYTES {
        return None;
    }
    let (kind, payload_start, payload_end, next_offset) = read_slice_box(trailer, 0)?;
    if kind != *b"mfra" || next_offset != trailer.len() {
        return Some(false);
    }
    let mfra_size = u32::try_from(trailer.len()).ok()?;
    Some(is_valid_terminal_mfra_payload(
        &trailer[payload_start..payload_end],
        mfra_size,
    ))
}

fn is_valid_terminal_mfra_payload(payload: &[u8], mfra_size: u32) -> bool {
    let mut has_tfra = false;
    let mut has_mfro = false;
    let valid = visit_child_boxes(payload, |kind, child_payload| {
        if has_mfro {
            return None;
        }
        match &kind {
            b"tfra" => {
                parse_tfra_index(child_payload)?;
                has_tfra = true;
                Some(())
            }
            b"mfro" => {
                if !has_tfra || parse_mfro_size(child_payload)? != mfra_size {
                    return None;
                }
                has_mfro = true;
                Some(())
            }
            _ => None,
        }
    })
    .is_some();
    valid && has_tfra && has_mfro
}

fn parse_tfra_index(payload: &[u8]) -> Option<()> {
    let version = *payload.first()?;
    if !matches!(version, 0 | 1) || full_box_flags(payload)? != 0 {
        return None;
    }
    if read_u32(payload, 4)? == 0 {
        return None;
    }
    let length_sizes = read_u32(payload, 8)?;
    if length_sizes >> 6 != 0 {
        return None;
    }
    let entry_count = usize::try_from(read_u32(payload, 12)?).ok()?;
    let variable_length = usize::try_from(1 + ((length_sizes >> 4) & 0x03))
        .ok()?
        .checked_add(usize::try_from(1 + ((length_sizes >> 2) & 0x03)).ok()?)?
        .checked_add(usize::try_from(1 + (length_sizes & 0x03)).ok()?)?;
    let fixed_length = if version == 0 { 8_usize } else { 16_usize };
    let entry_length = fixed_length.checked_add(variable_length)?;
    let entries_length = entry_count.checked_mul(entry_length)?;
    if 16_usize.checked_add(entries_length)? != payload.len() {
        return None;
    }
    Some(())
}

fn parse_mfro_size(payload: &[u8]) -> Option<u32> {
    if payload.len() != 8 || payload[..4] != [0, 0, 0, 0] {
        return None;
    }
    read_u32(payload, 4)
}

fn parse_timing_context(
    file: &mut File,
    initialization_length: u64,
    total_length: u64,
) -> io::Result<Option<Mp4TimingContext>> {
    let mut offset = 0;
    let mut context = Mp4TimingContext::default();
    while offset < initialization_length {
        let Some(header) = read_box_header(file, offset, total_length)? else {
            return Ok(None);
        };
        let Some(end) = header.end() else {
            return Ok(None);
        };
        if end > initialization_length {
            return Ok(None);
        }
        if header.kind == *b"moov" {
            let Some(payload) =
                read_box_payload_limited(file, header, MAX_TIMING_BOX_PAYLOAD_LENGTH)?
            else {
                return Ok(None);
            };
            if parse_moov_timing(&payload, &mut context).is_none() {
                return Ok(None);
            }
        }
        offset = end;
    }

    if !context.timescale_by_track.is_empty() {
        Ok(Some(context))
    } else {
        Ok(None)
    }
}

fn parse_moov_timing(payload: &[u8], context: &mut Mp4TimingContext) -> Option<()> {
    visit_child_boxes(payload, |kind, child_payload| match &kind {
        b"trak" => parse_trak_timing(child_payload, context),
        b"mvex" => parse_mvex_defaults(child_payload, context),
        _ => Some(()),
    })
}

fn parse_trak_timing(payload: &[u8], context: &mut Mp4TimingContext) -> Option<()> {
    let mut track_id = None;
    let mut timescale = None;
    visit_child_boxes(payload, |kind, child_payload| match &kind {
        b"tkhd" => {
            let parsed = parse_tkhd_track_id(child_payload)?;
            if parsed == 0 {
                return None;
            }
            match track_id {
                Some(existing) if existing != parsed => None,
                Some(_) => Some(()),
                None => {
                    track_id = Some(parsed);
                    Some(())
                }
            }
        }
        b"mdia" => {
            let Some(parsed) = parse_mdia_timescale(child_payload)? else {
                return Some(());
            };
            if parsed == 0 {
                return None;
            }
            match timescale {
                Some(existing) if existing != parsed => None,
                Some(_) => Some(()),
                None => {
                    timescale = Some(parsed);
                    Some(())
                }
            }
        }
        _ => Some(()),
    })?;
    let track_id = track_id?;
    let timescale = timescale?;
    match context.timescale_by_track.get(&track_id).copied() {
        Some(existing) if existing != timescale => None,
        Some(_) => Some(()),
        None => {
            context.timescale_by_track.insert(track_id, timescale);
            Some(())
        }
    }
}

fn parse_tkhd_track_id(payload: &[u8]) -> Option<u32> {
    let version = *payload.first()?;
    let offset = match version {
        0 => 12,
        1 => 20,
        _ => return None,
    };
    read_u32(payload, offset)
}

fn parse_mdia_timescale(payload: &[u8]) -> Option<Option<u32>> {
    let mut timescale = None;
    visit_child_boxes(payload, |kind, child_payload| {
        if kind != *b"mdhd" {
            return Some(());
        }
        let parsed = parse_mdhd_timescale(child_payload)?;
        if parsed == 0 {
            return None;
        }
        match timescale {
            Some(existing) if existing != parsed => None,
            Some(_) => Some(()),
            None => {
                timescale = Some(parsed);
                Some(())
            }
        }
    })?;
    Some(timescale)
}

fn parse_mdhd_timescale(payload: &[u8]) -> Option<u32> {
    let version = *payload.first()?;
    let offset = match version {
        0 => 12,
        1 => 20,
        _ => return None,
    };
    read_u32(payload, offset)
}

fn parse_mvex_defaults(payload: &[u8], context: &mut Mp4TimingContext) -> Option<()> {
    visit_child_boxes(payload, |kind, child_payload| {
        if kind != *b"trex" {
            return Some(());
        }
        let (track_id, default_sample_duration) =
            parse_trex_default_sample_duration(child_payload)?;
        if default_sample_duration == 0 {
            return Some(());
        }
        match context
            .default_sample_duration_by_track
            .get(&track_id)
            .copied()
        {
            Some(existing) if existing != default_sample_duration => None,
            Some(_) => Some(()),
            None => {
                context
                    .default_sample_duration_by_track
                    .insert(track_id, default_sample_duration);
                Some(())
            }
        }
    })
}

fn parse_trex_default_sample_duration(payload: &[u8]) -> Option<(u32, u32)> {
    let track_id = read_u32(payload, 4)?;
    let default_sample_duration = read_u32(payload, 12)?;
    Some((track_id, default_sample_duration))
}

fn parse_moof_duration_millis(
    file: &mut File,
    header: Mp4BoxHeader,
    timing: &Mp4TimingContext,
) -> io::Result<Option<u64>> {
    let Some(payload) = read_box_payload_limited(file, header, MAX_TIMING_BOX_PAYLOAD_LENGTH)?
    else {
        return Ok(None);
    };
    Ok(parse_moof_duration_millis_from_payload(&payload, timing))
}

fn parse_moof_duration_millis_from_payload(
    payload: &[u8],
    timing: &Mp4TimingContext,
) -> Option<u64> {
    let mut duration_units_by_track = HashMap::new();
    visit_child_boxes(payload, |kind, child_payload| {
        if kind != *b"traf" {
            return Some(());
        }
        let (traf_track_id, traf_duration_units) =
            parse_traf_duration_units(child_payload, timing)?;
        if !timing.timescale_by_track.contains_key(&traf_track_id) {
            return None;
        }
        let total_duration = duration_units_by_track
            .entry(traf_track_id)
            .or_insert(0_u64);
        *total_duration = total_duration.checked_add(traf_duration_units)?;
        Some(())
    })?;
    let mut duration_millis = None;
    for (track_id, duration_units) in duration_units_by_track {
        let timescale = timing
            .timescale_by_track
            .get(&track_id)
            .copied()
            .filter(|timescale| *timescale > 0)?;
        let track_duration_millis = duration_units
            .checked_mul(1_000)
            .and_then(|duration| duration.checked_add(u64::from(timescale) - 1))
            .map(|duration| duration / u64::from(timescale))?;
        duration_millis = Some(duration_millis.unwrap_or(0_u64).max(track_duration_millis));
    }
    duration_millis.filter(|duration| *duration > 0)
}

fn parse_traf_duration_units(payload: &[u8], timing: &Mp4TimingContext) -> Option<(u32, u64)> {
    let mut track_id = None;
    let mut default_sample_duration = None;
    let mut duration_units = 0_u64;
    visit_child_boxes(payload, |kind, child_payload| match &kind {
        b"tfhd" => {
            let parsed = parse_tfhd(child_payload)?;
            track_id = Some(parsed.track_id);
            default_sample_duration = parsed.default_sample_duration.or_else(|| {
                timing
                    .default_sample_duration_by_track
                    .get(&parsed.track_id)
                    .copied()
            });
            Some(())
        }
        b"trun" => {
            let track_id = track_id?;
            let trun_duration = parse_trun_duration_units(child_payload, default_sample_duration)?;
            duration_units = duration_units.checked_add(trun_duration)?;
            if timing
                .default_sample_duration_by_track
                .contains_key(&track_id)
                || default_sample_duration.is_some()
                || trun_has_sample_duration(child_payload)
            {
                Some(())
            } else {
                None
            }
        }
        _ => Some(()),
    })?;
    let track_id = track_id?;
    (duration_units > 0).then_some((track_id, duration_units))
}

struct Tfhd {
    track_id: u32,
    default_sample_duration: Option<u32>,
}

fn parse_tfhd(payload: &[u8]) -> Option<Tfhd> {
    let flags = full_box_flags(payload)?;
    let track_id = read_u32(payload, 4)?;
    let mut offset = 8_usize;
    if flags & 0x000001 != 0 {
        offset = offset.checked_add(8)?;
    }
    if flags & 0x000002 != 0 {
        offset = offset.checked_add(4)?;
    }
    let default_sample_duration = if flags & 0x000008 != 0 {
        let duration = read_u32(payload, offset)?;
        Some(duration).filter(|duration| *duration > 0)
    } else {
        None
    };
    Some(Tfhd {
        track_id,
        default_sample_duration,
    })
}

fn parse_trun_duration_units(payload: &[u8], default_sample_duration: Option<u32>) -> Option<u64> {
    let flags = full_box_flags(payload)?;
    let sample_count = u64::from(read_u32(payload, 4)?);
    let sample_duration_present = flags & 0x000100 != 0;
    let sample_size_present = flags & 0x000200 != 0;
    let sample_flags_present = flags & 0x000400 != 0;
    let sample_composition_time_offset_present = flags & 0x000800 != 0;
    let mut offset = 8_usize;
    if flags & 0x000001 != 0 {
        offset = offset.checked_add(4)?;
    }
    if flags & 0x000004 != 0 {
        offset = offset.checked_add(4)?;
    }
    let entry_bytes = (if sample_duration_present { 4_usize } else { 0 })
        + (if sample_size_present { 4 } else { 0 })
        + (if sample_flags_present { 4 } else { 0 })
        + (if sample_composition_time_offset_present {
            4
        } else {
            0
        });
    let sample_bytes = u64::try_from(entry_bytes).ok()?.checked_mul(sample_count)?;
    let payload_len = u64::try_from(payload.len()).ok()?;
    let offset_u64 = u64::try_from(offset).ok()?;
    if offset_u64.checked_add(sample_bytes)? > payload_len {
        return None;
    }
    if !sample_duration_present {
        let sample_duration = default_sample_duration?;
        return u64::from(sample_duration).checked_mul(sample_count);
    }

    let sample_count = usize::try_from(sample_count).ok()?;
    let mut duration_units = 0_u64;
    for _ in 0..sample_count {
        let sample_duration = read_u32(payload, offset)?;
        offset = offset.checked_add(4)?;
        duration_units = duration_units.checked_add(u64::from(sample_duration))?;
        if sample_size_present {
            offset = offset.checked_add(4)?;
        }
        if sample_flags_present {
            offset = offset.checked_add(4)?;
        }
        if sample_composition_time_offset_present {
            offset = offset.checked_add(4)?;
        }
        if offset > payload.len() {
            return None;
        }
    }
    Some(duration_units)
}

fn trun_has_sample_duration(payload: &[u8]) -> bool {
    full_box_flags(payload).is_some_and(|flags| flags & 0x000100 != 0)
}

fn find_fragment_end(
    file: &mut File,
    mut offset: u64,
    total_length: u64,
) -> io::Result<Option<u64>> {
    while offset < total_length {
        let Some(header) = read_box_header(file, offset, total_length)? else {
            return Ok(None);
        };
        match &header.kind {
            b"mdat" => return Ok(header.end()),
            b"moof" => return Ok(None),
            _ => {
                let Some(next_offset) = header.end() else {
                    return Ok(None);
                };
                offset = next_offset;
            }
        }
    }
    Ok(None)
}

fn read_box_header(
    file: &mut File,
    offset: u64,
    total_length: u64,
) -> io::Result<Option<Mp4BoxHeader>> {
    let Some(header_end) = offset.checked_add(8) else {
        return Ok(None);
    };
    if header_end > total_length {
        return Ok(None);
    }

    let mut bytes = [0_u8; 16];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes[..8])?;

    let size32 = u32::from_be_bytes(bytes[..4].try_into().expect("slice length is fixed"));
    let mut kind = [0_u8; 4];
    kind.copy_from_slice(&bytes[4..8]);
    let (header_length, size) = match size32 {
        0 => return Ok(None),
        1 => {
            let Some(large_header_end) = offset.checked_add(16) else {
                return Ok(None);
            };
            if large_header_end > total_length {
                return Ok(None);
            }
            file.read_exact(&mut bytes[8..16])?;
            (
                16_u64,
                u64::from_be_bytes(bytes[8..16].try_into().expect("slice length is fixed")),
            )
        }
        size => (8_u64, u64::from(size)),
    };
    if size < header_length {
        return Ok(None);
    }
    let Some(end) = offset.checked_add(size) else {
        return Ok(None);
    };
    if end > total_length {
        return Ok(None);
    }

    Ok(Some(Mp4BoxHeader {
        offset,
        header_length,
        size,
        kind,
    }))
}

fn read_box_payload_limited(
    file: &mut File,
    header: Mp4BoxHeader,
    max_payload_length: u64,
) -> io::Result<Option<Vec<u8>>> {
    let Some(payload_offset) = header.payload_offset() else {
        return Ok(None);
    };
    let Some(payload_length) = header.payload_length() else {
        return Ok(None);
    };
    if payload_length > max_payload_length {
        return Ok(None);
    }
    let Ok(payload_length) = usize::try_from(payload_length) else {
        return Ok(None);
    };
    let mut payload = vec![0_u8; payload_length];
    file.seek(SeekFrom::Start(payload_offset))?;
    file.read_exact(&mut payload)?;
    Ok(Some(payload))
}

fn visit_child_boxes(
    payload: &[u8],
    mut visit: impl FnMut([u8; 4], &[u8]) -> Option<()>,
) -> Option<()> {
    let mut offset = 0_usize;
    while offset < payload.len() {
        let (kind, payload_start, payload_end, next_offset) = read_slice_box(payload, offset)?;
        visit(kind, &payload[payload_start..payload_end])?;
        offset = next_offset;
    }
    Some(())
}

fn read_slice_box(payload: &[u8], offset: usize) -> Option<([u8; 4], usize, usize, usize)> {
    let header_end = offset.checked_add(8)?;
    if header_end > payload.len() {
        return None;
    }
    let size32 = u32::from_be_bytes(payload[offset..offset + 4].try_into().ok()?);
    let mut kind = [0_u8; 4];
    kind.copy_from_slice(&payload[offset + 4..offset + 8]);
    let (header_length, size) = if size32 == 1 {
        let large_header_end = offset.checked_add(16)?;
        if large_header_end > payload.len() {
            return None;
        }
        (
            16_usize,
            usize::try_from(u64::from_be_bytes(
                payload[offset + 8..offset + 16].try_into().ok()?,
            ))
            .ok()?,
        )
    } else if size32 == 0 {
        return None;
    } else {
        (8_usize, usize::try_from(size32).ok()?)
    };
    if size < header_length {
        return None;
    }
    let next_offset = offset.checked_add(size)?;
    if next_offset > payload.len() {
        return None;
    }
    Some((kind, offset + header_length, next_offset, next_offset))
}

fn full_box_flags(payload: &[u8]) -> Option<u32> {
    if payload.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes([0, payload[1], payload[2], payload[3]]))
}

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        payload
            .get(offset..offset.checked_add(4)?)?
            .try_into()
            .ok()?,
    ))
}

fn read_u16(payload: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        payload
            .get(offset..offset.checked_add(2)?)?
            .try_into()
            .ok()?,
    ))
}

fn read_u64(payload: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        payload
            .get(offset..offset.checked_add(8)?)?
            .try_into()
            .ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{Seek as _, SeekFrom, Write as _};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn computes_rfc_hls_target_duration_and_peak_bandwidth() {
        let short_tail = [
            Mp4SegmentRange {
                offset: 0,
                length: 32_000,
                duration_millis: 2_000,
            },
            Mp4SegmentRange {
                offset: 32_000,
                length: 2_000,
                duration_millis: 100,
            },
        ];
        assert_eq!(Some(2), hls_target_duration_for_ranges(&short_tail));
        assert_eq!(Some(129_524), hls_peak_bandwidth_for_ranges(&short_tail, 2));

        let boundary_windows = [
            Mp4SegmentRange {
                offset: 0,
                length: 1_000,
                duration_millis: 1_000,
            },
            Mp4SegmentRange {
                offset: 1_000,
                length: 1_000,
                duration_millis: 1_500,
            },
            Mp4SegmentRange {
                offset: 2_000,
                length: 3_000,
                duration_millis: 1_500,
            },
        ];
        assert_eq!(
            Some(16_000),
            hls_peak_bandwidth_for_ranges(&boundary_windows, 2)
        );
    }

    #[test]
    fn hls_bandwidth_work_is_bounded_and_typical_ranges_fit() {
        let typical = vec![
            Mp4SegmentRange {
                offset: 0,
                length: 100_000,
                duration_millis: 2_000,
            };
            3_600
        ];
        assert_eq!(Some(2), hls_target_duration_for_ranges(&typical));
        assert!(hls_peak_bandwidth_for_ranges(&typical, 2).is_some());

        let dense = vec![
            Mp4SegmentRange {
                offset: 0,
                length: 1,
                duration_millis: 1,
            };
            1_500
        ];
        assert_eq!(Some(1), hls_target_duration_for_ranges(&dense));
        assert_eq!(None, hls_peak_bandwidth_for_ranges(&dense, 1));
        let over_limit = vec![typical[0]; MAX_HLS_BANDWIDTH_RANGES + 1];
        assert_eq!(None, hls_target_duration_for_ranges(&over_limit));
    }

    #[test]
    fn hls_peak_bandwidth_rejects_invalid_and_overflowing_ranges() {
        assert_eq!(None, hls_target_duration_for_ranges(&[]));
        assert_eq!(None, hls_peak_bandwidth_for_ranges(&[], 2));
        assert_eq!(
            None,
            hls_peak_bandwidth_for_ranges(
                &[Mp4SegmentRange {
                    offset: 0,
                    length: 0,
                    duration_millis: 1_000,
                }],
                1,
            )
        );
        assert_eq!(
            None,
            hls_peak_bandwidth_for_ranges(
                &[Mp4SegmentRange {
                    offset: 0,
                    length: u64::MAX,
                    duration_millis: 1_000,
                }],
                1,
            )
        );
        assert_eq!(
            None,
            hls_peak_bandwidth_for_ranges(
                &[Mp4SegmentRange {
                    offset: 0,
                    length: 1,
                    duration_millis: 1,
                }],
                u64::MAX,
            )
        );
    }

    #[test]
    fn reads_fragment_ranges_from_multiple_moof_mdat_pairs() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("fragmented.mp4");
        let bytes = fragmented_mp4(&[
            (1_000, b"first-media".as_slice()),
            (2_000, b"second-media".as_slice()),
        ]);
        std::fs::write(&path, &bytes).expect("test mp4 should be written");
        let initialization_length = init_mp4().len() as u64;

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert_eq!(
            vec![
                Mp4SegmentRange {
                    offset: initialization_length,
                    length: (moof_box(1_000).len() + mp4_box(*b"mdat", b"first-media").len())
                        as u64,
                    duration_millis: 1_000,
                },
                Mp4SegmentRange {
                    offset: initialization_length
                        + (moof_box(1_000).len() + mp4_box(*b"mdat", b"first-media").len()) as u64,
                    length: (moof_box(2_000).len() + mp4_box(*b"mdat", b"second-media").len())
                        as u64,
                    duration_millis: 2_000,
                },
            ],
            ranges
        );
    }

    #[test]
    fn includes_fragment_prelude_boxes_in_following_range() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("fragmented.mp4");
        let mut bytes = init_mp4();
        let initialization_length = bytes.len() as u64;
        bytes.extend(mp4_box(*b"styp", b"cmaf"));
        bytes.extend(moof_box(1_000));
        bytes.extend(mp4_box(*b"mdat", b"first-media"));
        bytes.extend(moof_box(1_000));
        bytes.extend(mp4_box(*b"mdat", b"second-media"));
        std::fs::write(&path, &bytes).expect("test mp4 should be written");

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert_eq!(2, ranges.len());
        assert_eq!(initialization_length, ranges[0].offset);
        assert_eq!(
            (mp4_box(*b"styp", b"cmaf").len()
                + moof_box(1_000).len()
                + mp4_box(*b"mdat", b"first-media").len()) as u64,
            ranges[0].length
        );
    }

    #[test]
    fn reads_fragment_durations_from_multiple_tracks() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("fragmented-av.mp4");
        let bytes = multi_track_fragmented_mp4(&[
            (1_000, 48_480, b"first-media".as_slice()),
            (2_000, 96_000, b"second-media".as_slice()),
        ]);
        std::fs::write(&path, &bytes).expect("test mp4 should be written");
        let initialization_length = multi_track_init_mp4().len() as u64;

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert_eq!(
            vec![
                Mp4SegmentRange {
                    offset: initialization_length,
                    length: (moof_box_with_tracks(&[(1, 1_000), (2, 48_480)]).len()
                        + mp4_box(*b"mdat", b"first-media").len())
                        as u64,
                    duration_millis: 1_010,
                },
                Mp4SegmentRange {
                    offset: initialization_length
                        + (moof_box_with_tracks(&[(1, 1_000), (2, 48_480)]).len()
                            + mp4_box(*b"mdat", b"first-media").len())
                            as u64,
                    length: (moof_box_with_tracks(&[(1, 2_000), (2, 96_000)]).len()
                        + mp4_box(*b"mdat", b"second-media").len())
                        as u64,
                    duration_millis: 2_000,
                },
            ],
            ranges
        );
    }

    #[test]
    fn sums_multiple_traf_durations_for_same_track_before_selecting_longest_track() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("fragmented-split-traf.mp4");
        let mut bytes = multi_track_init_mp4();
        let initialization_length = bytes.len() as u64;
        bytes.extend(moof_box_with_tracks(&[(1, 400), (1, 600), (2, 47_000)]));
        bytes.extend(mp4_box(*b"mdat", b"first-media"));
        bytes.extend(moof_box_with_tracks(&[(1, 1_000), (2, 48_000)]));
        bytes.extend(mp4_box(*b"mdat", b"second-media"));
        std::fs::write(&path, &bytes).expect("test mp4 should be written");

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert_eq!(2, ranges.len());
        assert_eq!(1_000, ranges[0].duration_millis);
    }

    #[test]
    fn returns_no_index_for_single_fragment() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("single-fragment.mp4");
        let mut bytes = fragmented_mp4(&[(1_000, b"only-media".as_slice())]);
        bytes.extend(valid_mfra());
        std::fs::write(&path, &bytes).expect("test mp4 should be written");
        let initialization_length = init_mp4().len() as u64;

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert!(ranges.is_empty());
    }

    #[test]
    fn returns_no_index_when_fragment_timing_is_missing() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("untimed-fragmented.mp4");
        let mut bytes = untimed_init_mp4();
        let initialization_length = bytes.len() as u64;
        bytes.extend(mp4_box(*b"moof", b"first-moof"));
        bytes.extend(mp4_box(*b"mdat", b"first-media"));
        bytes.extend(mp4_box(*b"moof", b"second-moof"));
        bytes.extend(mp4_box(*b"mdat", b"second-media"));
        bytes.extend(valid_mfra());
        std::fs::write(&path, &bytes).expect("test mp4 should be written");

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert!(ranges.is_empty());
    }

    #[test]
    fn folds_valid_terminal_mfra_into_last_fragment_without_changing_timing() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("fragmented-with-mfra.mp4");
        let initialization_length = init_mp4().len() as u64;
        let first_fragment = [moof_box(1_000), mp4_box(*b"mdat", b"first-media")].concat();
        let second_fragment = [moof_box(2_000), mp4_box(*b"mdat", b"second-media")].concat();
        let mfra = valid_mfra();
        let mut bytes = init_mp4();
        bytes.extend(&first_fragment);
        bytes.extend(&second_fragment);
        bytes.extend(&mfra);
        let total_length = bytes.len() as u64;
        std::fs::write(&path, &bytes).expect("test mp4 should be written");

        let ranges = mp4_fragment_ranges(&path, initialization_length, total_length)
            .expect("fragment ranges should parse");

        assert_eq!(
            vec![
                Mp4SegmentRange {
                    offset: initialization_length,
                    length: first_fragment.len() as u64,
                    duration_millis: 1_000,
                },
                Mp4SegmentRange {
                    offset: initialization_length + first_fragment.len() as u64,
                    length: (second_fragment.len() + mfra.len()) as u64,
                    duration_millis: 2_000,
                },
            ],
            ranges
        );
        assert_eq!(
            total_length,
            ranges.last().unwrap().offset + ranges.last().unwrap().length
        );
        assert_eq!(
            total_length - initialization_length,
            ranges.iter().map(|r| r.length).sum::<u64>()
        );
    }

    #[test]
    fn rejects_malformed_or_unknown_terminal_mfra_children() {
        let mut bad_child_size = valid_mfra();
        bad_child_size[8..12].copy_from_slice(&7_u32.to_be_bytes());

        let mut truncated = valid_mfra();
        truncated.pop();

        let mut unknown_child = valid_mfra();
        unknown_child[12..16].copy_from_slice(b"free");

        let mut bad_mfra_size = valid_mfra();
        let mfro_size_offset = bad_mfra_size.len() - 4;
        bad_mfra_size[mfro_size_offset..].copy_from_slice(&1_u32.to_be_bytes());

        let tfra = tfra_box();
        let missing_mfro = mfra_box(&tfra, false);

        for (name, trailer) in [
            ("bad child size", bad_child_size),
            ("truncated trailer", truncated),
            ("unknown child", unknown_child),
            ("mfro size mismatch", bad_mfra_size),
            ("missing mfro", missing_mfro),
        ] {
            let temp = TempDir::new().expect("temp dir should be created");
            let path = temp.path().join("fragmented-with-invalid-mfra.mp4");
            let mut bytes = fragmented_mp4(&[
                (1_000, b"first-media".as_slice()),
                (2_000, b"second-media".as_slice()),
            ]);
            let initialization_length = init_mp4().len() as u64;
            bytes.extend(trailer);
            std::fs::write(&path, &bytes).expect("test mp4 should be written");

            let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
                .unwrap_or_else(|error| panic!("{name}: parsing should not fail: {error}"));

            assert!(ranges.is_empty(), "{name} should invalidate the index");
        }
    }

    #[test]
    fn returns_no_index_when_trailer_box_is_not_covered() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("fragmented-with-trailer.mp4");
        let mut bytes = fragmented_mp4(&[
            (1_000, b"first-media".as_slice()),
            (1_000, b"second-media".as_slice()),
        ]);
        let initialization_length = init_mp4().len() as u64;
        bytes.extend(mp4_box(*b"mfra", b"trailer"));
        std::fs::write(&path, &bytes).expect("test mp4 should be written");

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert!(ranges.is_empty());
    }

    #[test]
    fn returns_no_index_when_moof_payload_exceeds_limit() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("oversized-moof.mp4");
        let initialization = init_mp4();
        let initialization_length = initialization.len() as u64;
        let moof_size = MAX_TIMING_BOX_PAYLOAD_LENGTH + 9;
        let total_length = initialization_length + moof_size;
        let mut file = File::create(&path).expect("test mp4 should be created");
        file.write_all(&initialization)
            .expect("initialization should be written");
        file.seek(SeekFrom::Start(initialization_length))
            .expect("moof header should be seekable");
        file.write_all(
            &u32::try_from(moof_size)
                .expect("test moof size should fit u32")
                .to_be_bytes(),
        )
        .expect("moof size should be written");
        file.write_all(b"moof")
            .expect("moof kind should be written");
        file.set_len(total_length)
            .expect("oversized moof payload should be sparse");

        let ranges = mp4_fragment_ranges(&path, initialization_length, total_length)
            .expect("fragment range parsing should not fail");

        assert!(ranges.is_empty());
    }

    #[test]
    fn returns_no_index_for_mdat_without_moof() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("invalid.mp4");
        let mut bytes = init_mp4();
        let initialization_length = bytes.len() as u64;
        bytes.extend(mp4_box(*b"mdat", b"media"));
        std::fs::write(&path, &bytes).expect("test mp4 should be written");

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert!(ranges.is_empty());
    }

    #[test]
    fn trun_default_sample_duration_uses_constant_time_duration_sum() {
        let payload = trun_default_sample_count_payload(u32::MAX);

        let duration_units =
            parse_trun_duration_units(&payload, Some(2)).expect("duration should parse");

        assert_eq!(u64::from(u32::MAX) * 2, duration_units);
        assert!(parse_trun_duration_units(&payload, None).is_none());
    }

    #[test]
    fn trun_per_sample_duration_requires_payload_for_sample_count() {
        let mut payload = Vec::new();
        payload.extend([0, 0, 1, 0]);
        payload.extend(u32::MAX.to_be_bytes());

        assert!(parse_trun_duration_units(&payload, Some(1)).is_none());
    }

    #[test]
    fn parses_version_zero_index_with_first_offset_padding_and_sap_type_zero() {
        let init = mp4_box(*b"ftyp", b"isom");
        let padding = mp4_box(*b"free", b"pad");
        let sidx = sidx_box(
            0,
            1_000,
            5,
            u64::try_from(padding.len()).unwrap(),
            &[(7, 1_000, 0x8000_0000), (9, 2_000, 0x8000_0000)],
        );
        let first_offset = init.len() + sidx.len() + padding.len();
        let first = vec![0x11; 7];
        let second = [0x22; 9];
        let total_length = first_offset + first.len() + second.len();
        let mut prefix = [init.as_slice(), sidx.as_slice(), padding.as_slice()].concat();
        prefix.extend(&first);

        let index = mp4_sidx_index(
            &prefix,
            u64::try_from(init.len()).unwrap(),
            u64::try_from(total_length).unwrap(),
        )
        .expect("flat version zero sidx should parse");
        let ranges = mp4_sidx_segment_ranges(index, &[])
            .expect("exact-EOF references should finalize without a trailer");

        assert_eq!(2, ranges.len());
        assert_eq!(u64::try_from(init.len()).unwrap(), ranges[0].offset);
        assert_eq!(
            u64::try_from(sidx.len() + padding.len() + first.len()).unwrap(),
            ranges[0].length
        );
        assert_eq!(
            u64::try_from(first_offset + first.len()).unwrap(),
            ranges[1].offset
        );
        assert_eq!(u64::try_from(second.len()).unwrap(), ranges[1].length);
        assert_eq!(
            [1_000, 2_000],
            [ranges[0].duration_millis, ranges[1].duration_millis]
        );
        assert_eq!(
            u64::try_from(total_length - init.len()).unwrap(),
            ranges.iter().map(|range| range.length).sum::<u64>()
        );
    }

    #[test]
    fn parses_version_one_index_and_folds_only_valid_terminal_mfra() {
        let init = mp4_box(*b"ftyp", b"isom");
        let first = [0x31; 5];
        let second = [0x42; 6];
        let sidx = sidx_box(
            1,
            90_000,
            u64::MAX - 300_000,
            0,
            &[(5, 90_000, 0x8000_0000), (6, 180_000, 0x8000_0000)],
        );
        let first_offset = init.len() + sidx.len();
        let mut prefix = [init.as_slice(), sidx.as_slice()].concat();
        prefix.extend(first);
        let mfra = valid_mfra();
        let total_length = first_offset + first.len() + second.len() + mfra.len();
        let index = mp4_sidx_index(
            &prefix,
            u64::try_from(init.len()).unwrap(),
            u64::try_from(total_length).unwrap(),
        )
        .expect("flat version one sidx should parse");

        assert_eq!(
            u64::try_from(mfra.len()).unwrap(),
            index.trailing_length,
            "the index should expose the exact terminal metadata range"
        );
        assert_eq!(
            u64::try_from(first_offset + first.len() + second.len()).unwrap(),
            index.trailing_offset
        );
        let ranges = mp4_sidx_segment_ranges(index, &mfra)
            .expect("valid terminal mfra should fold into the final range");
        assert_eq!(2, ranges.len());
        assert_eq!(1_000, ranges[0].duration_millis);
        assert_eq!(2_000, ranges[1].duration_millis);
        assert_eq!(
            u64::try_from(total_length - init.len()).unwrap(),
            ranges.iter().map(|range| range.length).sum::<u64>()
        );
        assert_eq!(
            u64::try_from(total_length).unwrap(),
            ranges.last().unwrap().offset + ranges.last().unwrap().length
        );
    }

    #[test]
    fn accepts_only_supported_sap_types_with_zero_delta() {
        let init = mp4_box(*b"ftyp", b"isom");
        for sap in [0x8000_0000, 0x9000_0000, 0xa000_0000] {
            let sidx = sidx_box(0, 1_000, 0, 0, &[(1, 1, sap), (1, 1, 0x8000_0000)]);
            let total = init.len() + sidx.len() + 2;
            let prefix = [init.as_slice(), sidx.as_slice()].concat();
            let index = mp4_sidx_index(
                &prefix,
                u64::try_from(init.len()).unwrap(),
                u64::try_from(total).unwrap(),
            )
            .expect("SAP types zero, one and two should be accepted");
            assert!(mp4_sidx_segment_ranges(index, &[]).is_some());
        }
    }

    #[test]
    fn rejects_sidx_references_without_independent_zero_delta_sap() {
        let init = mp4_box(*b"ftyp", b"isom");
        for (name, sap) in [
            ("missing starts_with_SAP", 0),
            ("unsupported SAP type", 0xb000_0000),
            ("nonzero SAP delta", 0x8000_0001),
        ] {
            let sidx = sidx_box(0, 1_000, 0, 0, &[(1, 1, sap), (1, 1, 0x8000_0000)]);
            let total = init.len() + sidx.len() + 2;
            let prefix = [init.as_slice(), sidx.as_slice()].concat();
            assert!(
                mp4_sidx_index(
                    &prefix,
                    u64::try_from(init.len()).unwrap(),
                    u64::try_from(total).unwrap(),
                )
                .is_none(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_invalid_sidx_duration_size_hierarchy_overflow_and_truncation() {
        let cases = [
            ("zero duration", 1_000, 0, 0_u32, 0_u64, u64::MAX, false),
            ("zero size", 1_000, 1, 0_u32, 0_u64, u64::MAX, false),
            ("hierarchical", 1_000, 1, 1_u32, 0_u64, u64::MAX, true),
            (
                "timescale overflow",
                1_000,
                1,
                1_u32,
                0_u64,
                u64::MAX,
                false,
            ),
        ];
        for (name, timescale, duration, size, offset, earliest, hierarchical) in cases {
            let init = mp4_box(*b"ftyp", b"isom");
            let type_size = size | if hierarchical { 0x8000_0000 } else { 0 };
            let sidx = sidx_box(
                1,
                timescale,
                earliest,
                offset,
                &[(type_size, duration, 0x8000_0000), (1, 1, 0x8000_0000)],
            );
            let total = init.len() + sidx.len() + 2;
            let prefix = [init.as_slice(), sidx.as_slice()].concat();
            let parsed = mp4_sidx_index(
                &prefix,
                u64::try_from(init.len()).unwrap(),
                u64::try_from(total).unwrap(),
            );
            assert!(parsed.is_none(), "{name} should be rejected");
        }

        let init = mp4_box(*b"ftyp", b"isom");
        let sidx = sidx_box(
            1,
            1_000,
            0,
            u64::MAX,
            &[(1, 1, 0x8000_0000), (1, 1, 0x8000_0000)],
        );
        let total = init.len() + sidx.len() + 2;
        assert!(
            mp4_sidx_index(
                &[init.as_slice(), sidx.as_slice()].concat(),
                u64::try_from(init.len()).unwrap(),
                u64::try_from(total).unwrap(),
            )
            .is_none(),
            "reference offset overflow should be rejected"
        );

        let mut truncated = [init.as_slice(), sidx.as_slice()].concat();
        truncated.pop();
        assert!(
            mp4_sidx_index(
                &truncated,
                u64::try_from(init.len()).unwrap(),
                u64::try_from(total).unwrap(),
            )
            .is_none(),
            "truncated sidx should be rejected"
        );
    }

    #[test]
    fn rejects_unknown_or_missing_terminal_mfra_and_excessive_reference_count() {
        let init = mp4_box(*b"ftyp", b"isom");
        let first = [0x11; 3];
        let second = [0x22; 4];
        let sidx = sidx_box(0, 1_000, 0, 0, &[(3, 1, 0x8000_0000), (4, 1, 0x8000_0000)]);
        let prefix = [init.as_slice(), sidx.as_slice(), &first].concat();
        let index_offset = u64::try_from(init.len()).unwrap();
        let data_end = init.len() + sidx.len() + first.len() + second.len();

        for (name, trailer) in [
            ("unknown tail", mp4_box(*b"free", b"junk")),
            ("missing mfro", mfra_box(&tfra_box(), false)),
        ] {
            let total = data_end + trailer.len();
            let index = mp4_sidx_index(&prefix, index_offset, u64::try_from(total).unwrap())
                .expect("the flat references should parse before trailer validation");
            assert!(
                mp4_sidx_segment_ranges(index, &trailer).is_none(),
                "{name} should be rejected"
            );
        }

        let references = vec![(1, 1, 0x8000_0000); MAX_SIDX_REFERENCES + 1];
        let excessive = sidx_box(0, 1_000, 0, 0, &references);
        let total = init.len() + excessive.len() + references.len();
        assert!(
            mp4_sidx_index(
                &[init.as_slice(), excessive.as_slice()].concat(),
                index_offset,
                u64::try_from(total).unwrap(),
            )
            .is_none()
        );

        assert!(
            mp4_sidx_index(
                &vec![0; MAX_SIDX_PREFIX_BYTES + 1],
                0,
                u64::try_from(MAX_SIDX_PREFIX_BYTES + 2).unwrap(),
            )
            .is_none()
        );

        let small = sidx_box(0, 1_000, 0, 0, &[(1, 1, 0x8000_0000), (1, 1, 0x8000_0000)]);
        let excessive_trailer_total =
            init.len() + small.len() + 2 + usize::try_from(MAX_SIDX_TRAILER_BYTES).unwrap() + 1;
        assert!(
            mp4_sidx_index(
                &[init.as_slice(), small.as_slice()].concat(),
                index_offset,
                u64::try_from(excessive_trailer_total).unwrap(),
            )
            .is_none()
        );

        let second_index = sidx_box(0, 1_000, 0, 0, &[(1, 1, 0x8000_0000), (1, 1, 0x8000_0000)]);
        let first_index = sidx_box(
            0,
            1_000,
            0,
            u64::try_from(second_index.len()).unwrap(),
            &[(1, 1, 0x8000_0000), (1, 1, 0x8000_0000)],
        );
        let total = init.len() + first_index.len() + second_index.len() + 2;
        assert!(
            mp4_sidx_index(
                &[
                    init.as_slice(),
                    first_index.as_slice(),
                    second_index.as_slice()
                ]
                .concat(),
                index_offset,
                u64::try_from(total).unwrap(),
            )
            .is_none()
        );
    }

    fn fragmented_mp4(fragments: &[(u32, &[u8])]) -> Vec<u8> {
        let mut bytes = init_mp4();
        for (duration, mdat) in fragments {
            bytes.extend(moof_box(*duration));
            bytes.extend(mp4_box(*b"mdat", mdat));
        }
        bytes
    }

    fn multi_track_fragmented_mp4(fragments: &[(u32, u32, &[u8])]) -> Vec<u8> {
        let mut bytes = multi_track_init_mp4();
        for (video_duration, audio_duration, mdat) in fragments {
            bytes.extend(moof_box_with_tracks(&[
                (1, *video_duration),
                (2, *audio_duration),
            ]));
            bytes.extend(mp4_box(*b"mdat", mdat));
        }
        bytes
    }

    fn init_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", &trak_box(1, 1_000)));
        bytes
    }

    fn multi_track_init_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(
            *b"moov",
            &[trak_box(1, 1_000), trak_box(2, 48_000)].concat(),
        ));
        bytes
    }

    fn trak_box(track_id: u32, timescale: u32) -> Vec<u8> {
        mp4_box(
            *b"trak",
            &[tkhd_box(track_id), mp4_box(*b"mdia", &mdhd_box(timescale))].concat(),
        )
    }

    fn tkhd_box(track_id: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend([0, 0, 0, 0]);
        payload.extend(0_u32.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(track_id.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(0_u64.to_be_bytes());
        mp4_box(*b"tkhd", &payload)
    }

    fn untimed_init_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes
    }

    fn mdhd_box(timescale: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend([0, 0, 0, 0]);
        payload.extend(0_u32.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(timescale.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(0_u16.to_be_bytes());
        payload.extend(0_u16.to_be_bytes());
        mp4_box(*b"mdhd", &payload)
    }

    fn moof_box(duration: u32) -> Vec<u8> {
        moof_box_with_tracks(&[(1, duration)])
    }

    fn moof_box_with_tracks(track_durations: &[(u32, u32)]) -> Vec<u8> {
        mp4_box(
            *b"moof",
            &track_durations
                .iter()
                .map(|(track_id, duration)| {
                    mp4_box(
                        *b"traf",
                        &[tfhd_box(*track_id), trun_box(*duration)].concat(),
                    )
                })
                .collect::<Vec<_>>()
                .concat(),
        )
    }

    fn tfhd_box(track_id: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend([0, 0, 0, 0]);
        payload.extend(track_id.to_be_bytes());
        mp4_box(*b"tfhd", &payload)
    }

    fn trun_box(duration: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend([0, 0, 1, 0]);
        payload.extend(1_u32.to_be_bytes());
        payload.extend(duration.to_be_bytes());
        mp4_box(*b"trun", &payload)
    }

    fn trun_default_sample_count_payload(sample_count: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend([0, 0, 0, 0]);
        payload.extend(sample_count.to_be_bytes());
        payload
    }

    fn mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).unwrap();
        let mut bytes = Vec::new();
        bytes.extend(size.to_be_bytes());
        bytes.extend(kind);
        bytes.extend(payload);
        bytes
    }

    fn sidx_box(
        version: u8,
        timescale: u32,
        earliest: u64,
        first_offset: u64,
        references: &[(u32, u32, u32)],
    ) -> Vec<u8> {
        let mut payload = vec![version, 0, 0, 0];
        payload.extend(1_u32.to_be_bytes());
        payload.extend(timescale.to_be_bytes());
        match version {
            0 => {
                payload.extend(u32::try_from(earliest).unwrap().to_be_bytes());
                payload.extend(u32::try_from(first_offset).unwrap().to_be_bytes());
            }
            1 => {
                payload.extend(earliest.to_be_bytes());
                payload.extend(first_offset.to_be_bytes());
            }
            _ => panic!("test fixture only supports sidx versions zero and one"),
        }
        payload.extend(0_u16.to_be_bytes());
        payload.extend(u16::try_from(references.len()).unwrap().to_be_bytes());
        for (size_and_type, duration, sap) in references {
            payload.extend(size_and_type.to_be_bytes());
            payload.extend(duration.to_be_bytes());
            payload.extend(sap.to_be_bytes());
        }
        mp4_box(*b"sidx", &payload)
    }

    fn valid_mfra() -> Vec<u8> {
        mfra_box(&tfra_box(), true)
    }

    fn tfra_box() -> Vec<u8> {
        let mut payload = vec![0, 0, 0, 0];
        payload.extend(1_u32.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(1_u32.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend([1, 1, 1]);
        mp4_box(*b"tfra", &payload)
    }

    fn mfra_box(tfra: &[u8], include_mfro: bool) -> Vec<u8> {
        let mfra_size = 8 + tfra.len() + if include_mfro { 16 } else { 0 };
        let mut payload = tfra.to_vec();
        if include_mfro {
            let mut mfro_payload = vec![0, 0, 0, 0];
            mfro_payload.extend(u32::try_from(mfra_size).unwrap().to_be_bytes());
            payload.extend(mp4_box(*b"mfro", &mfro_payload));
        }
        mp4_box(*b"mfra", &payload)
    }
}
