use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Mp4SegmentRange {
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

#[derive(Clone, Copy)]
struct Mp4BoxHeader {
    offset: u64,
    size: u64,
    kind: [u8; 4],
}

impl Mp4BoxHeader {
    fn end(self) -> Option<u64> {
        self.offset.checked_add(self.size)
    }
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

    if segments.len() < 2 {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(segments))
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

    Ok(Some(Mp4BoxHeader { offset, size, kind }))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn reads_fragment_ranges_from_multiple_moof_mdat_pairs() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("fragmented.mp4");
        let bytes = fragmented_mp4(&[
            (b"first-moof".as_slice(), b"first-media".as_slice()),
            (b"second-moof".as_slice(), b"second-media".as_slice()),
        ]);
        std::fs::write(&path, &bytes).expect("test mp4 should be written");
        let initialization_length =
            (mp4_box(*b"ftyp", b"isom").len() + mp4_box(*b"moov", b"metadata").len()) as u64;

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert_eq!(
            vec![
                Mp4SegmentRange {
                    offset: initialization_length,
                    length: (mp4_box(*b"moof", b"first-moof").len()
                        + mp4_box(*b"mdat", b"first-media").len())
                        as u64,
                },
                Mp4SegmentRange {
                    offset: initialization_length
                        + (mp4_box(*b"moof", b"first-moof").len()
                            + mp4_box(*b"mdat", b"first-media").len())
                            as u64,
                    length: (mp4_box(*b"moof", b"second-moof").len()
                        + mp4_box(*b"mdat", b"second-media").len())
                        as u64,
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
        bytes.extend(mp4_box(*b"moof", b"first-moof"));
        bytes.extend(mp4_box(*b"mdat", b"first-media"));
        bytes.extend(mp4_box(*b"moof", b"second-moof"));
        bytes.extend(mp4_box(*b"mdat", b"second-media"));
        std::fs::write(&path, &bytes).expect("test mp4 should be written");

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

        assert_eq!(2, ranges.len());
        assert_eq!(initialization_length, ranges[0].offset);
        assert_eq!(
            (mp4_box(*b"styp", b"cmaf").len()
                + mp4_box(*b"moof", b"first-moof").len()
                + mp4_box(*b"mdat", b"first-media").len()) as u64,
            ranges[0].length
        );
    }

    #[test]
    fn returns_no_index_for_single_fragment() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("single-fragment.mp4");
        let bytes = fragmented_mp4(&[(b"only-moof".as_slice(), b"only-media".as_slice())]);
        std::fs::write(&path, &bytes).expect("test mp4 should be written");
        let initialization_length = init_mp4().len() as u64;

        let ranges = mp4_fragment_ranges(&path, initialization_length, bytes.len() as u64)
            .expect("fragment ranges should parse");

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

    fn fragmented_mp4(fragments: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut bytes = init_mp4();
        for (moof, mdat) in fragments {
            bytes.extend(mp4_box(*b"moof", moof));
            bytes.extend(mp4_box(*b"mdat", mdat));
        }
        bytes
    }

    fn init_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes
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
