use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_TIMING_BOX_PAYLOAD_LENGTH: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Mp4SegmentRange {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) duration_millis: u64,
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

#[cfg(test)]
mod tests {
    use std::io::{Seek as _, SeekFrom, Write as _};

    use tempfile::TempDir;

    use super::*;

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
        let bytes = fragmented_mp4(&[(1_000, b"only-media".as_slice())]);
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
        std::fs::write(&path, &bytes).expect("test mp4 should be written");

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert!(ranges.is_empty());
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
}
