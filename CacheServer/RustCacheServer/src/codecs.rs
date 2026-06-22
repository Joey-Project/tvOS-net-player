pub(crate) fn codec_list_matches(codecs: &str, predicate: fn(&str) -> bool) -> bool {
    codecs.split(',').any(predicate)
}

pub(crate) fn is_h264_codec(codec: &str) -> bool {
    let codec = codec.trim().to_ascii_lowercase();
    codec.starts_with("avc1") || codec.starts_with("avc3")
}

pub(crate) fn is_aac_codec(codec: &str) -> bool {
    let codec = codec.trim().to_ascii_lowercase();
    let Some(audio_object_type) = codec.strip_prefix("mp4a.40.") else {
        return false;
    };

    audio_object_type
        .parse::<u8>()
        .is_ok_and(|audio_object_type| {
            matches!(
                audio_object_type,
                1 | 2 | 3 | 4 | 5 | 6 | 17 | 19 | 20 | 23 | 29 | 39 | 42
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_aac_codec_strings() {
        assert!(is_aac_codec("mp4a.40.2"));
        assert!(is_aac_codec(" mp4a.40.5 "));
        assert!(is_aac_codec("MP4A.40.29"));
        assert!(is_aac_codec("mp4a.40.42"));
    }

    #[test]
    fn rejects_non_aac_mp4a_codec_strings() {
        assert!(!is_aac_codec("mp4a"));
        assert!(!is_aac_codec("mp4a.6B"));
        assert!(!is_aac_codec("mp4a.40.34"));
        assert!(!is_aac_codec("flac"));
    }
}
