use std::{io::SeekFrom, sync::Arc};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{
        HeaderMap, HeaderValue, Response, StatusCode,
        header::{
            ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LAST_MODIFIED, RANGE,
        },
    },
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::{AppState, library::OpenedMediaFile};

#[derive(Clone)]
pub struct MediaState {
    state: Arc<AppState>,
}

impl MediaState {
    pub fn new(state: AppState) -> Self {
        Self {
            state: Arc::new(state),
        }
    }
}

pub async fn media_get(
    State(state): State<MediaState>,
    Path((item_id, variant_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    media_response(state, item_id, variant_id, headers, false).await
}

pub async fn media_head(
    State(state): State<MediaState>,
    Path((item_id, variant_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    media_response(state, item_id, variant_id, headers, true).await
}

async fn media_response(
    state: MediaState,
    item_id: String,
    variant_id: String,
    headers: HeaderMap,
    head_only: bool,
) -> Response<Body> {
    let Some(opened_file) = state
        .state
        .library
        .open_media_file(&item_id, &variant_id)
        .await
    else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    let range = match parse_range(headers.get(RANGE), opened_file.size_bytes) {
        Ok(range) => range,
        Err(_) => {
            let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
            response.headers_mut().insert(
                CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{}", opened_file.size_bytes))
                    .expect("content range header should be valid"),
            );
            return response;
        }
    };

    build_file_response(opened_file, range, head_only).await
}

async fn build_file_response(
    opened_file: OpenedMediaFile,
    range: Option<ByteRange>,
    head_only: bool,
) -> Response<Body> {
    let size = opened_file.size_bytes;
    let (status, start, length, content_range) = if let Some(range) = range {
        (
            StatusCode::PARTIAL_CONTENT,
            range.start,
            range.length(),
            Some(format!("bytes {}-{}/{}", range.start, range.end, size)),
        )
    } else {
        (StatusCode::OK, 0, size, None)
    };

    let body = if head_only {
        Body::empty()
    } else {
        let mut file = tokio::fs::File::from_std(opened_file.file);
        if file.seek(SeekFrom::Start(start)).await.is_err() {
            return empty_response(StatusCode::NOT_FOUND);
        }

        Body::from_stream(ReaderStream::new(file.take(length)))
    };

    let mut response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, opened_file.content_type)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, length.to_string())
        .header(
            LAST_MODIFIED,
            httpdate::fmt_http_date(opened_file.last_modified),
        )
        .body(body)
        .expect("media response should build");

    if let Some(content_range) = content_range {
        response.headers_mut().insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&content_range).expect("content range header should be valid"),
        );
    }

    response
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("empty response should build")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    fn length(self) -> u64 {
        self.end - self.start + 1
    }
}

fn parse_range(header: Option<&HeaderValue>, size: u64) -> Result<Option<ByteRange>, ()> {
    let Some(header) = header else {
        return Ok(None);
    };
    let value = header.to_str().map_err(|_| ())?;
    let Some(spec) = value.strip_prefix("bytes=") else {
        return Err(());
    };
    if spec.contains(',') || size == 0 {
        return Err(());
    }

    let (start, end) = spec.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix_length = end.parse::<u64>().map_err(|_| ())?;
        if suffix_length == 0 {
            return Err(());
        }

        let start = size.saturating_sub(suffix_length);
        return Ok(Some(ByteRange {
            start,
            end: size - 1,
        }));
    }

    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }

    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(size - 1)
    };
    if end < start {
        return Err(());
    }

    Ok(Some(ByteRange { start, end }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_ranges() {
        assert_eq!(Some(ByteRange { start: 0, end: 3 }), parse("bytes=0-3", 16));
        assert_eq!(Some(ByteRange { start: 4, end: 15 }), parse("bytes=4-", 16));
        assert_eq!(
            Some(ByteRange { start: 12, end: 15 }),
            parse("bytes=-4", 16)
        );
    }

    #[test]
    fn rejects_unsatisfiable_ranges() {
        assert!(parse_range(Some(&HeaderValue::from_static("bytes=99-100")), 16).is_err());
        assert!(parse_range(Some(&HeaderValue::from_static("items=0-1")), 16).is_err());
        assert!(parse_range(Some(&HeaderValue::from_static("bytes=0-1,2-3")), 16).is_err());
    }

    fn parse(value: &'static str, size: u64) -> Option<ByteRange> {
        parse_range(Some(&HeaderValue::from_static(value)), size).unwrap()
    }
}
