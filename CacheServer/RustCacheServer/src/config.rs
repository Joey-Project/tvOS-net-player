use std::{
    env,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
};

use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheServerOptions {
    pub server_id: String,
    pub server_name: String,
    pub root_path: PathBuf,
    pub grpc_listen_url: String,
    pub media_listen_url: String,
    pub public_media_base_uri: Option<String>,
    pub allowed_extensions: Vec<String>,
}

impl Default for CacheServerOptions {
    fn default() -> Self {
        Self {
            server_id: "default".to_owned(),
            server_name: "TVOS Net Player Cache".to_owned(),
            root_path: env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|parent| parent.join("cache")))
                .unwrap_or_else(|| PathBuf::from("cache")),
            grpc_listen_url: "http://localhost:50051".to_owned(),
            media_listen_url: "http://localhost:8080".to_owned(),
            public_media_base_uri: None,
            allowed_extensions: vec![".mp4".to_owned(), ".m4v".to_owned(), ".mov".to_owned()],
        }
    }
}

impl CacheServerOptions {
    pub fn from_args<I>(args: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut options = Self::default();
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            let Some(key) = arg.strip_prefix("--") else {
                return Err(ConfigError::new(format!("unexpected argument: {arg}")));
            };
            let value = iter
                .next()
                .ok_or_else(|| ConfigError::new(format!("missing value for argument: {arg}")))?;
            options.apply(key, value)?;
        }

        options.validate()?;
        Ok(options)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let grpc_url = parse_http_url(&self.grpc_listen_url)?;
        let media_url = parse_http_url(&self.media_listen_url)?;
        if grpc_url.port_or_known_default() == media_url.port_or_known_default() {
            return Err(ConfigError::new(
                "gRPC and media listen URLs must use distinct ports in this slice.",
            ));
        }

        Ok(())
    }

    pub fn grpc_listen_addr(&self) -> Result<SocketAddr, ConfigError> {
        listen_addrs(&self.grpc_listen_url)?
            .into_iter()
            .next()
            .ok_or_else(|| ConfigError::new("listen URL produced no socket addresses"))
    }

    pub fn grpc_listen_addrs(&self) -> Result<Vec<SocketAddr>, ConfigError> {
        listen_addrs(&self.grpc_listen_url)
    }

    pub fn media_listen_addr(&self) -> Result<SocketAddr, ConfigError> {
        listen_addrs(&self.media_listen_url)?
            .into_iter()
            .next()
            .ok_or_else(|| ConfigError::new("listen URL produced no socket addresses"))
    }

    pub fn media_listen_addrs(&self) -> Result<Vec<SocketAddr>, ConfigError> {
        listen_addrs(&self.media_listen_url)
    }

    fn apply(&mut self, key: &str, value: String) -> Result<(), ConfigError> {
        match key {
            "Cache:ServerId" => self.server_id = value,
            "Cache:ServerName" => self.server_name = value,
            "Cache:RootPath" => self.root_path = PathBuf::from(value),
            "Cache:GrpcListenUrl" => self.grpc_listen_url = value,
            "Cache:MediaListenUrl" => self.media_listen_url = value,
            "Cache:PublicMediaBaseUri" => self.public_media_base_uri = Some(value),
            "Cache:AllowedExtensions" => {
                self.allowed_extensions = value
                    .split(',')
                    .map(str::trim)
                    .filter(|extension| !extension.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
            }
            key if key.starts_with("Logging:") => {}
            _ => return Err(ConfigError::new(format!("unknown argument: --{key}"))),
        }

        Ok(())
    }
}

pub fn normalize_listen_host(url: &Url) -> String {
    let Some(host) = url.host_str() else {
        return "localhost".to_owned();
    };

    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned()
}

fn listen_addrs(listen_url: &str) -> Result<Vec<SocketAddr>, ConfigError> {
    let url = parse_http_url(listen_url)?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ConfigError::new(format!("missing port in listen URL: {listen_url}")))?;
    let host = normalize_listen_host(&url);
    let ips = match host.as_str() {
        "localhost" => vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ],
        "0.0.0.0" | "::" | "*" | "+" => vec![
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ],
        _ => vec![host.parse().map_err(|_| {
            ConfigError::new(format!(
                "listen host must be localhost or an IP address: {host}"
            ))
        })?],
    };

    Ok(ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect())
}

fn parse_http_url(value: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(value)
        .map_err(|error| ConfigError::new(format!("invalid listen URL {value}: {error}")))?;
    if url.scheme() != "http" {
        return Err(ConfigError::new(
            "Only cleartext http listen URLs are supported in this slice.",
        ));
    }

    Ok(url)
}

#[derive(Debug)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_existing_cache_server_style_args() {
        let options = CacheServerOptions::from_args([
            "--Cache:GrpcListenUrl".to_owned(),
            "http://127.0.0.1:51000".to_owned(),
            "--Cache:MediaListenUrl".to_owned(),
            "http://127.0.0.1:51001".to_owned(),
            "--Cache:RootPath".to_owned(),
            "/tmp/cache".to_owned(),
            "--Cache:ServerName".to_owned(),
            "Test Cache".to_owned(),
        ])
        .expect("options should parse");

        assert_eq!("Test Cache", options.server_name);
        assert_eq!(PathBuf::from("/tmp/cache"), options.root_path);
        assert_eq!(
            "127.0.0.1:51000".parse::<SocketAddr>().unwrap(),
            options.grpc_listen_addr().unwrap()
        );
    }

    #[test]
    fn rejects_https_urls() {
        let options = CacheServerOptions {
            grpc_listen_url: "https://localhost:50051".to_owned(),
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[test]
    fn rejects_same_listener_ports() {
        let options = CacheServerOptions {
            media_listen_url: "http://localhost:50051".to_owned(),
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[test]
    fn localhost_listen_urls_bind_ipv4_and_ipv6_loopback() {
        let options = CacheServerOptions::default();

        assert_eq!(
            vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 50051),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 50051),
            ],
            options.grpc_listen_addrs().unwrap()
        );
    }

    #[test]
    fn wildcard_listen_urls_bind_ipv4_and_ipv6_unspecified() {
        for host in ["0.0.0.0", "[::]", "*", "+"] {
            let options = CacheServerOptions {
                grpc_listen_url: format!("http://{host}:50051"),
                ..CacheServerOptions::default()
            };

            assert_eq!(
                vec![
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 50051),
                    SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 50051),
                ],
                options.grpc_listen_addrs().unwrap()
            );
        }
    }
}
