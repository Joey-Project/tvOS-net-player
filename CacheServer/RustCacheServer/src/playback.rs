use std::{net::IpAddr, sync::Arc};

use tonic::Request;
use url::Url;

use crate::config::{CacheServerOptions, normalize_listen_host};

#[derive(Clone)]
pub struct PlaybackUriFactory {
    options: Arc<CacheServerOptions>,
}

impl PlaybackUriFactory {
    pub fn new(options: Arc<CacheServerOptions>) -> Self {
        Self { options }
    }

    pub fn create<T>(&self, request: &Request<T>, item_id: &str, variant_id: &str) -> String {
        let base_uri = self.create_base_uri(request);

        format!(
            "{}/media/{}/{}",
            base_uri.trim_end_matches('/'),
            urlencoding::encode(item_id),
            urlencoding::encode(variant_id)
        )
    }

    pub fn create_hls_master_playlist<T>(&self, request: &Request<T>, session_id: &str) -> String {
        let base_uri = self.create_base_uri(request);

        format!(
            "{}/hls/{}/master.m3u8",
            base_uri.trim_end_matches('/'),
            urlencoding::encode(session_id)
        )
    }

    fn create_base_uri<T>(&self, request: &Request<T>) -> String {
        self.options
            .public_media_base_uri
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let authority = request
                    .metadata()
                    .get("host")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("localhost");
                Self::create_media_base_uri_for_request(
                    authority,
                    request.local_addr().map(|addr| addr.ip()),
                    &self.options.media_listen_url,
                )
            })
    }

    pub fn create_media_base_uri(request_authority: &str, media_listen_url: &str) -> String {
        Self::create_media_base_uri_for_request(request_authority, None, media_listen_url)
    }

    pub fn create_media_base_uri_for_request(
        request_authority: &str,
        local_ip: Option<IpAddr>,
        media_listen_url: &str,
    ) -> String {
        let media_uri = Url::parse(media_listen_url).expect("media listen URL must be valid");
        let listen_host = normalize_listen_host(&media_uri);
        let host = match listen_host.as_str() {
            "0.0.0.0" | "::" | "*" | "+" => local_ip
                .filter(|ip| !ip.is_unspecified())
                .map(|ip| format_uri_host(&ip.to_string()))
                .unwrap_or_else(|| extract_uri_host(request_authority)),
            _ => format_uri_host(&listen_host),
        };
        let port = media_uri
            .port_or_known_default()
            .expect("media listen URL must have a port");

        format!("{}://{}:{port}", media_uri.scheme(), host)
    }
}

fn extract_uri_host(authority: &str) -> String {
    let authority = authority.trim();
    if authority.is_empty() {
        return "localhost".to_owned();
    }

    if authority.starts_with('[') {
        return authority
            .find(']')
            .map(|index| authority[..=index].to_owned())
            .unwrap_or_else(|| "localhost".to_owned());
    }

    let colon_count = authority
        .chars()
        .filter(|character| *character == ':')
        .count();
    let host = if colon_count == 1 {
        authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(authority)
    } else {
        authority
    };

    format_uri_host(host)
}

fn format_uri_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_wildcard_listen_url_with_request_host() {
        assert_eq!(
            "http://192.168.1.10:8080",
            PlaybackUriFactory::create_media_base_uri("192.168.1.10:50051", "http://0.0.0.0:8080")
        );
    }

    #[test]
    fn prefers_local_socket_address_for_wildcard_listeners() {
        assert_eq!(
            "http://10.0.0.5:8080",
            PlaybackUriFactory::create_media_base_uri_for_request(
                "localhost:50051",
                Some("10.0.0.5".parse().unwrap()),
                "http://0.0.0.0:8080",
            )
        );
    }

    #[test]
    fn formats_ipv6_hosts() {
        assert_eq!(
            "http://[::1]:8080",
            PlaybackUriFactory::create_media_base_uri("[::1]:50051", "http://0.0.0.0:8080")
        );
        assert_eq!(
            "http://[::1]:8080",
            PlaybackUriFactory::create_media_base_uri("localhost:50051", "http://[::1]:8080")
        );
    }
}
