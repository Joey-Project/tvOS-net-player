use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Response, StatusCode, header},
    middleware,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tonic::{Code, Status};
use url::Url;

use crate::{config::CacheServerOptions, media::MediaState};

const MAX_LOGIN_PAGE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LOGIN_COMPLETION_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginCompletion {
    capability: String,
    message_origin: String,
    message: String,
}

pub(crate) fn router() -> Router<MediaState> {
    Router::new()
        .route("/login/bilibili/{session_id}", get(login_page))
        .route(
            "/login/bilibili/{session_id}/complete",
            post(login_complete).layer(DefaultBodyLimit::max(MAX_LOGIN_COMPLETION_BYTES)),
        )
        .layer(middleware::map_response(secure_response))
}

async fn secure_response(mut response: Response<Body>) -> Response<Body> {
    set_private_response_headers(response.headers_mut());
    response
}

pub(crate) async fn login_page(
    State(state): State<MediaState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let (manager, options) = state.bilibili_login_context();
    let Some(origin) = admitted_origin(options, &headers, false) else {
        return fixed_response(StatusCode::FORBIDDEN, "Login origin is not allowed.");
    };
    match manager.browser_page(&session_id, &origin) {
        Ok(page) if page.len() <= MAX_LOGIN_PAGE_BYTES => {
            let policy = page_content_security_policy(&page);
            let mut response = fixed_response(StatusCode::OK, &page);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
            );
            let Ok(policy) = axum::http::HeaderValue::from_str(&policy) else {
                return fixed_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Login page unavailable.",
                );
            };
            response
                .headers_mut()
                .insert(header::CONTENT_SECURITY_POLICY, policy);
            response
        }
        Ok(_) => fixed_response(StatusCode::INTERNAL_SERVER_ERROR, "Login page unavailable."),
        Err(error) => status_response(error),
    }
}

pub(crate) async fn login_complete(
    State(state): State<MediaState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (manager, options) = state.bilibili_login_context();
    let Some(origin) = admitted_origin(options, &headers, true) else {
        return fixed_response(StatusCode::FORBIDDEN, "Login origin is not allowed.");
    };
    if body.len() > MAX_LOGIN_COMPLETION_BYTES
        || headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value != "application/json")
    {
        return fixed_response(StatusCode::BAD_REQUEST, "Invalid login completion.");
    }
    let Ok(completion) = serde_json::from_slice::<LoginCompletion>(&body) else {
        return fixed_response(StatusCode::BAD_REQUEST, "Invalid login completion.");
    };
    match manager
        .complete_browser_login(
            &session_id,
            &origin,
            &completion.capability,
            &completion.message_origin,
            &completion.message,
        )
        .await
    {
        Ok(()) => fixed_response(StatusCode::NO_CONTENT, ""),
        Err(error) => status_response(error),
    }
}

fn admitted_origin(
    options: &CacheServerOptions,
    headers: &HeaderMap,
    require_origin: bool,
) -> Option<String> {
    if !options.allow_bilibili_login_sessions {
        return None;
    }
    let base = options.bilibili_login_base_uri()?;
    let expected = Url::parse(&base).ok()?;
    let host = headers.get(header::HOST)?.to_str().ok()?;
    if host.len() > 1024 || host.contains('@') {
        return None;
    }
    let authority = host.parse::<axum::http::uri::Authority>().ok()?;
    let candidate = Url::parse(&format!("{}://{authority}/", expected.scheme())).ok()?;
    if candidate.origin() != expected.origin()
        || !candidate.username().is_empty()
        || candidate.password().is_some()
        || candidate.path() != "/"
        || candidate.query().is_some()
        || candidate.fragment().is_some()
    {
        return None;
    }
    let origin = expected.origin().ascii_serialization();
    match headers.get(header::ORIGIN) {
        Some(value) if value.to_str().ok() == Some(origin.as_str()) => Some(origin),
        None if !require_origin => Some(origin),
        _ => None,
    }
}

fn page_content_security_policy(page: &str) -> String {
    let mut script_sources = String::new();
    let mut remaining = page;
    while let Some((_, script_and_tail)) = remaining.split_once("<script>") {
        let Some((script, tail)) = script_and_tail.split_once("</script>") else {
            break;
        };
        script_sources.push_str(" 'sha256-");
        script_sources.push_str(&STANDARD.encode(Sha256::digest(script.as_bytes())));
        script_sources.push('\'');
        remaining = tail;
    }
    if script_sources.is_empty() {
        script_sources.push_str(" 'none'");
    }
    format!(
        "default-src 'none'; script-src{script_sources}; connect-src 'self'; style-src 'unsafe-inline'; base-uri 'none'; object-src 'none'; form-action 'none'; frame-ancestors 'none'"
    )
}

fn status_response(status: Status) -> Response<Body> {
    let code = match status.code() {
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::PermissionDenied | Code::Unauthenticated => StatusCode::FORBIDDEN,
        Code::InvalidArgument => StatusCode::BAD_REQUEST,
        Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        Code::DeadlineExceeded => StatusCode::GONE,
        Code::Aborted | Code::AlreadyExists | Code::FailedPrecondition => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    fixed_response(
        code,
        "Login could not be completed. Check the server and retry.",
    )
}

fn fixed_response(code: StatusCode, body: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(body.to_owned()));
    *response.status_mut() = code;
    let headers = response.headers_mut();
    set_private_response_headers(headers);
    headers.insert(
        header::CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().expect("static header"),
    );
    response
}

fn set_private_response_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        "no-referrer".parse().expect("static header"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static header"),
    );
    headers.entry(header::CONTENT_SECURITY_POLICY).or_insert(
        "default-src 'none'; base-uri 'none'; frame-ancestors 'none'"
            .parse()
            .expect("static header"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> CacheServerOptions {
        CacheServerOptions {
            allow_bilibili_login_sessions: true,
            public_media_base_uri: Some("https://cache.example.test/prefix".to_owned()),
            ..CacheServerOptions::default()
        }
    }

    fn headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().expect("test host"));
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().expect("test origin"));
        }
        headers
    }

    #[test]
    fn callback_requires_configured_host_and_exact_origin() {
        let options = options();
        assert!(admitted_origin(&options, &headers("cache.example.test", None), false).is_some());
        assert!(admitted_origin(&options, &headers("cache.example.test", None), true).is_none());
        assert!(
            admitted_origin(
                &options,
                &headers("cache.example.test:443", Some("https://cache.example.test")),
                true
            )
            .is_some()
        );
        for host in [
            "evil.example.test",
            "user@cache.example.test",
            "cache.example.test:80",
        ] {
            assert!(
                admitted_origin(
                    &options,
                    &headers(host, Some("https://cache.example.test")),
                    true
                )
                .is_none()
            );
        }
        for origin in [
            "null",
            "http://cache.example.test",
            "https://evil.example.test",
            "https://cache.example.test/",
        ] {
            assert!(
                admitted_origin(&options, &headers("cache.example.test", Some(origin)), true)
                    .is_none()
            );
        }
        let disabled = CacheServerOptions {
            allow_bilibili_login_sessions: false,
            ..options
        };
        assert!(
            admitted_origin(
                &disabled,
                &headers("cache.example.test", Some("https://cache.example.test")),
                true
            )
            .is_none()
        );
    }

    #[test]
    fn explicit_ipv6_http_origin_matches_host() {
        let options = CacheServerOptions {
            allow_bilibili_login_sessions: true,
            public_media_base_uri: Some("http://[::1]:8080".to_owned()),
            ..CacheServerOptions::default()
        };
        assert_eq!(
            Some("http://[::1]:8080".to_owned()),
            admitted_origin(
                &options,
                &headers("[::1]:8080", Some("http://[::1]:8080")),
                true
            )
        );
    }

    #[test]
    fn page_policy_only_authorizes_exact_inline_script_bytes() {
        let policy = page_content_security_policy("<script>const ready = true;</script>");
        let expected = STANDARD.encode(Sha256::digest(b"const ready = true;"));
        assert!(policy.contains(&format!("'sha256-{expected}'")));
        assert!(!policy.contains("script-src 'unsafe-inline'"));
        assert!(page_content_security_policy("<p>Ready</p>").contains("script-src 'none'"));
        assert!(policy.contains("frame-ancestors 'none'"));
    }

    #[tokio::test]
    async fn error_responses_hide_provider_messages_and_disallow_caching() {
        let response = status_response(Status::invalid_argument("provider-private-detail"));
        assert_eq!(StatusCode::BAD_REQUEST, response.status());
        assert_eq!("no-store", response.headers()[header::CACHE_CONTROL]);
        assert_eq!("no-referrer", response.headers()[header::REFERRER_POLICY]);
        assert_eq!(
            "nosniff",
            response.headers()[header::X_CONTENT_TYPE_OPTIONS]
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("small error body");
        assert!(!String::from_utf8_lossy(&bytes).contains("provider-private-detail"));
    }

    #[tokio::test]
    async fn http_routes_bound_completion_body_and_keep_rejections_private() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let address = listener.local_addr().expect("test address");
        let origin = format!("http://{address}");
        let options = CacheServerOptions {
            allow_bilibili_login_sessions: true,
            bbdown_credential_path: Some(temporary.path().join("credentials.json")),
            public_media_base_uri: Some(origin.clone()),
            root_path: temporary.path().to_path_buf(),
            task_state_path: temporary.path().join("tasks.json"),
            ..CacheServerOptions::default()
        };
        let routes = router().with_state(MediaState::new(crate::AppState::new(options)));
        let server = tokio::spawn(async move { axum::serve(listener, routes).await });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test HTTP client");
        let completion_url = format!("{origin}/login/bilibili/unknown/complete");
        let oversized = client
            .post(&completion_url)
            .header(header::ORIGIN, &origin)
            .header(header::CONTENT_TYPE, "application/json")
            .body("x".repeat(MAX_LOGIN_COMPLETION_BYTES + 1))
            .send()
            .await
            .expect("oversized request");
        assert_eq!(StatusCode::PAYLOAD_TOO_LARGE, oversized.status());
        assert_eq!("no-store", oversized.headers()[header::CACHE_CONTROL]);
        assert_eq!("no-referrer", oversized.headers()[header::REFERRER_POLICY]);
        assert_eq!(
            "nosniff",
            oversized.headers()[header::X_CONTENT_TYPE_OPTIONS]
        );

        let rejected = client
            .post(&completion_url)
            .header(header::ORIGIN, "https://untrusted.example.test")
            .header(header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .expect("cross-origin request");
        assert_eq!(StatusCode::FORBIDDEN, rejected.status());

        let malformed = client
            .post(&completion_url)
            .header(header::ORIGIN, &origin)
            .header(header::CONTENT_TYPE, "application/json")
            .body("{\"capability\": \"provider-private-detail\", \"extra\": true}")
            .send()
            .await
            .expect("malformed request");
        assert_eq!(StatusCode::BAD_REQUEST, malformed.status());
        assert!(
            !malformed
                .text()
                .await
                .expect("safe error body")
                .contains("provider-private-detail")
        );

        let unknown = client
            .get(format!("{origin}/login/bilibili/unknown"))
            .send()
            .await
            .expect("unknown session");
        assert_eq!(StatusCode::NOT_FOUND, unknown.status());
        assert_eq!("no-store", unknown.headers()[header::CACHE_CONTROL]);
        server.abort();
        assert!(
            server
                .await
                .expect_err("server should be cancelled")
                .is_cancelled()
        );
    }
}
