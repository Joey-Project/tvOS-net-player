use std::collections::HashSet;

use prost::Message;
use tonic::Status;

use crate::{
    generated::tvos_net_player::v1::{HlsRenditionCatalog, HlsRenditionDescriptor},
    hls::{HlsPlaybackSession, HlsVariant},
};

pub(crate) const MAX_HLS_CATALOG_RENDITIONS: usize = 128;
pub(crate) const MAX_HLS_CATALOG_ENCODED_BYTES: usize = 64 * 1024;
const MAX_VARIANT_ID_BYTES: usize = 256;
const MAX_FORMAT_HINT_BYTES: usize = 256;
const MAX_MIME_HINT_BYTES: usize = 128;

pub(crate) fn build_catalog(
    session: &HlsPlaybackSession,
    generation: u64,
    variants: &[&HlsVariant],
) -> Result<HlsRenditionCatalog, Status> {
    if generation == 0 || !valid_identifier(&session.id) {
        return Err(Status::failed_precondition(
            "The registered HLS session identity or generation is invalid.",
        ));
    }
    validate_advertised_session(session, &session.id)?;
    if variants.is_empty() {
        return Err(Status::failed_precondition(
            "No complete HLS representation is available for cataloging.",
        ));
    }

    let mut renditions = Vec::with_capacity(variants.len());
    for variant in variants {
        if !valid_identifier(&variant.id) {
            return Err(Status::failed_precondition(
                "The registered HLS session contains an invalid rendition identifier.",
            ));
        }
        if variant.bandwidth == 0 {
            return Err(Status::failed_precondition(
                "The registered HLS session contains an invalid advertised bandwidth.",
            ));
        }

        renditions.push(HlsRenditionDescriptor {
            variant_id: variant.id.clone(),
            advertised_bandwidth_bits_per_second: variant.bandwidth,
            video_resource_bandwidth_bits_per_second: positive_rate(
                variant.video.request.bandwidth,
            ),
            audio_resource_bandwidth_bits_per_second: variant
                .audio
                .as_ref()
                .and_then(|audio| positive_rate(audio.request.bandwidth)),
            has_separate_audio: variant.audio.is_some(),
            video_resource_codecs: valid_format_hint(variant.video.request.codecs.as_deref()),
            audio_resource_codecs: variant
                .audio
                .as_ref()
                .and_then(|audio| valid_format_hint(audio.request.codecs.as_deref())),
            video_resource_mime_type: valid_mime_hint(variant.video.request.mime_type.as_deref()),
            audio_resource_mime_type: variant
                .audio
                .as_ref()
                .and_then(|audio| valid_mime_hint(audio.request.mime_type.as_deref())),
            advertised_width: paired_positive_dimension(variant.width, variant.height)
                .map(|(width, _)| width),
            advertised_height: paired_positive_dimension(variant.width, variant.height)
                .map(|(_, height)| height),
        });
    }

    let catalog = HlsRenditionCatalog {
        session_id: session.id.clone(),
        session_generation: generation,
        renditions,
    };
    if catalog.encoded_len() > MAX_HLS_CATALOG_ENCODED_BYTES {
        return Err(Status::resource_exhausted(
            "The HLS rendition catalog exceeds the encoded response limit.",
        ));
    }
    Ok(catalog)
}

pub(crate) fn validate_advertised_session<'a>(
    session: &'a HlsPlaybackSession,
    expected_session_id: &str,
) -> Result<Vec<&'a HlsVariant>, Status> {
    if session.id != expected_session_id {
        return Err(Status::failed_precondition(
            "The registered HLS session identity is inconsistent.",
        ));
    }
    if session.advertised_variant_count() > MAX_HLS_CATALOG_RENDITIONS {
        return Err(Status::resource_exhausted(
            "The HLS rendition catalog exceeds the rendition limit.",
        ));
    }
    let variants = session.advertised_variants_with_filter(|_| true);
    let mut identifiers = HashSet::with_capacity(variants.len());
    for variant in &variants {
        if !valid_identifier(&variant.id) || !identifiers.insert(variant.id.as_str()) {
            return Err(Status::failed_precondition(
                "The registered HLS session contains an invalid or duplicate rendition identifier.",
            ));
        }
        if variant.bandwidth == 0 {
            return Err(Status::failed_precondition(
                "The registered HLS session contains an invalid advertised bandwidth.",
            ));
        }
    }
    Ok(variants)
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VARIANT_ID_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn positive_rate(value: Option<u64>) -> Option<u64> {
    value.filter(|rate| *rate > 0)
}

fn paired_positive_dimension(width: Option<u32>, height: Option<u32>) -> Option<(u32, u32)> {
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Some((width, height)),
        _ => None,
    }
}

fn valid_format_hint(value: Option<&str>) -> Option<String> {
    let value = value?;
    if value.is_empty() || value.len() > MAX_FORMAT_HINT_BYTES || !value.is_ascii() {
        return None;
    }
    if value.starts_with(' ') || value.ends_with(' ') {
        return None;
    }
    for token in value.split(',') {
        let token = token.trim_matches(' ');
        if token.is_empty()
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return None;
        }
    }
    Some(value.to_owned())
}

fn valid_mime_hint(value: Option<&str>) -> Option<String> {
    let value = value?;
    if value.is_empty() || value.len() > MAX_MIME_HINT_BYTES || !value.is_ascii() {
        return None;
    }
    let mut slashes = 0usize;
    let mut token_has_byte = false;
    let mut side_has_byte = [false, false];
    let mut side = 0usize;
    for byte in value.bytes() {
        if byte == b'/' {
            slashes += 1;
            if slashes > 1 || !side_has_byte[0] {
                return None;
            }
            side = 1;
            token_has_byte = false;
        } else if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
            )
        {
            token_has_byte = true;
            side_has_byte[side] = true;
        } else {
            return None;
        }
    }
    (slashes == 1 && token_has_byte && side_has_byte[1]).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_hints_require_nonempty_ascii_tokens_and_bounded_length() {
        assert_eq!(
            Some("avc1.640028, mp4a.40.2".to_owned()),
            valid_format_hint(Some("avc1.640028, mp4a.40.2"))
        );
        for invalid in [
            "",
            " avc1",
            "avc1 ",
            "avc1,,mp4a",
            "avc1, ,mp4a",
            "avc1 mp4a",
            "https://host",
            "avc1\tfoo",
            "é",
        ] {
            assert!(valid_format_hint(Some(invalid)).is_none(), "{invalid:?}");
        }
        assert!(valid_format_hint(Some(&"a".repeat(MAX_FORMAT_HINT_BYTES + 1))).is_none());
    }

    #[test]
    fn mime_hints_are_bare_single_slash_ascii_types() {
        assert_eq!(
            Some("video/mp4".to_owned()),
            valid_mime_hint(Some("video/mp4"))
        );
        for invalid in [
            "",
            " video/mp4",
            "video/mp4 ",
            "video",
            "/mp4",
            "video/",
            "video/mp4; codecs=x",
            "https://host/video",
            "video//mp4",
            "é/video",
        ] {
            assert!(valid_mime_hint(Some(invalid)).is_none(), "{invalid:?}");
        }
        assert!(valid_mime_hint(Some(&"a".repeat(MAX_MIME_HINT_BYTES + 1))).is_none());
    }

    #[test]
    fn identifier_validation_preserves_utf8_identity() {
        let id = "rendition-雪";
        assert!(valid_identifier(id));
        assert_eq!(id, id.to_owned());
        for invalid in [
            "",
            " leading",
            "trailing ",
            "\u{00a0}edge",
            "edge\u{2003}",
            "bad\u{0085}id",
        ] {
            assert!(!valid_identifier(invalid), "{invalid:?}");
        }
        assert!(!valid_identifier(&"x".repeat(MAX_VARIANT_ID_BYTES + 1)));
    }
}
