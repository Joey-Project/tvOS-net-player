use std::{
    env, fs, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Component, Path, PathBuf},
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
    pub task_state_path: PathBuf,
    pub allowed_extensions: Vec<String>,
    pub bilibili_worker_enabled: bool,
    pub bilibili_worker_max_concurrent_tasks: usize,
    pub bbdown_output_dir: Option<PathBuf>,
    pub bbdown_archive_path: Option<PathBuf>,
    pub bbdown_ffmpeg_path: PathBuf,
}

impl Default for CacheServerOptions {
    fn default() -> Self {
        let app_base_path = default_app_base_path();
        let state_path = app_base_path.join("cache-server-state");
        Self {
            server_id: "default".to_owned(),
            server_name: "TVOS Net Player Cache".to_owned(),
            root_path: app_base_path.join("cache"),
            grpc_listen_url: "http://localhost:50051".to_owned(),
            media_listen_url: "http://localhost:8080".to_owned(),
            public_media_base_uri: None,
            task_state_path: state_path.join("tasks.json"),
            allowed_extensions: vec![".mp4".to_owned(), ".m4v".to_owned(), ".mov".to_owned()],
            bilibili_worker_enabled: true,
            bilibili_worker_max_concurrent_tasks: 1,
            bbdown_output_dir: None,
            bbdown_archive_path: None,
            bbdown_ffmpeg_path: PathBuf::from("ffmpeg"),
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
        if self.bilibili_worker_max_concurrent_tasks == 0 {
            return Err(ConfigError::new(
                "Bilibili worker max concurrent tasks must be greater than zero.",
            ));
        }
        if self
            .bbdown_output_dir
            .as_deref()
            .is_some_and(path_contains_parent_component)
        {
            return Err(ConfigError::new(
                "BBDown output directory must not contain parent directory components.",
            ));
        }
        let root_path = self.normalized_root_path();
        let bbdown_output_dir = self.bbdown_output_dir();
        if !bbdown_output_dir.starts_with(&root_path) {
            return Err(ConfigError::new(
                "BBDown output directory must be inside Cache:RootPath.",
            ));
        }
        if bbdown_output_dir_contains_link(&root_path, &bbdown_output_dir)? {
            return Err(ConfigError::new(
                "BBDown output directory must not include symlink components inside Cache:RootPath.",
            ));
        }

        Ok(())
    }

    pub fn normalized_for_runtime(mut self) -> Self {
        self.root_path = self.normalized_root_path();
        if self.bbdown_output_dir.is_some() {
            self.bbdown_output_dir = Some(self.bbdown_output_dir());
        }
        self
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

    pub fn task_state_path(&self) -> PathBuf {
        self.task_state_path.clone()
    }

    pub fn normalized_root_path(&self) -> PathBuf {
        normalize_existing_path_prefix(&self.root_path)
    }

    pub fn bbdown_output_dir(&self) -> PathBuf {
        normalize_existing_path_prefix(&self.raw_bbdown_output_dir())
    }

    fn raw_bbdown_output_dir(&self) -> PathBuf {
        self.bbdown_output_dir
            .clone()
            .unwrap_or_else(|| self.root_path.join("Bilibili"))
    }

    pub fn bbdown_archive_path(&self) -> PathBuf {
        self.bbdown_archive_path.clone().unwrap_or_else(|| {
            self.task_state_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| default_app_base_path().join("cache-server-state"))
                .join("bbdown-archive.json")
        })
    }

    fn apply(&mut self, key: &str, value: String) -> Result<(), ConfigError> {
        match key {
            "Cache:ServerId" => self.server_id = value,
            "Cache:ServerName" => self.server_name = value,
            "Cache:RootPath" => self.root_path = PathBuf::from(value),
            "Cache:GrpcListenUrl" => self.grpc_listen_url = value,
            "Cache:MediaListenUrl" => self.media_listen_url = value,
            "Cache:PublicMediaBaseUri" => self.public_media_base_uri = Some(value),
            "Cache:TaskStatePath" => self.task_state_path = PathBuf::from(value),
            "Cache:AllowedExtensions" => {
                self.allowed_extensions = value
                    .split(',')
                    .map(str::trim)
                    .filter(|extension| !extension.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
            }
            "Cache:BilibiliWorkerEnabled" => self.bilibili_worker_enabled = parse_bool(&value)?,
            "Cache:BilibiliWorkerMaxConcurrentTasks" => {
                self.bilibili_worker_max_concurrent_tasks = value.parse().map_err(|_| {
                    ConfigError::new(format!(
                        "invalid integer for --Cache:BilibiliWorkerMaxConcurrentTasks: {value}"
                    ))
                })?;
            }
            "Cache:BBDownOutputDir" => self.bbdown_output_dir = Some(PathBuf::from(value)),
            "Cache:BBDownArchivePath" => self.bbdown_archive_path = Some(PathBuf::from(value)),
            "Cache:BBDownFfmpegPath" => self.bbdown_ffmpeg_path = PathBuf::from(value),
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

fn parse_bool(value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" | "on" => Ok(true),
        "false" | "0" | "no" | "n" | "off" => Ok(false),
        _ => Err(ConfigError::new(format!("invalid boolean value: {value}"))),
    }
}

fn normalized_absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn normalize_existing_path_prefix(path: &Path) -> PathBuf {
    let absolute = normalized_absolute_path(path);
    let mut existing_prefix = absolute.clone();
    let mut missing_components = Vec::new();

    while !existing_prefix.exists() {
        let Some(file_name) = existing_prefix.file_name().map(ToOwned::to_owned) else {
            break;
        };
        missing_components.push(file_name);
        if !existing_prefix.pop() {
            break;
        }
    }

    let mut normalized = existing_prefix
        .canonicalize()
        .unwrap_or_else(|_| existing_prefix.clone());
    for component in missing_components.iter().rev() {
        normalized.push(component);
    }

    normalized
}

fn path_contains_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn bbdown_output_dir_contains_link(
    root_path: &Path,
    output_dir: &Path,
) -> Result<bool, ConfigError> {
    let root_path = normalized_absolute_path(root_path);
    let output_dir = normalized_absolute_path(output_dir);
    if !output_dir.starts_with(&root_path) {
        return Ok(true);
    }

    if is_existing_link(&root_path)? {
        return Ok(true);
    }

    let Some(relative_output_dir) = output_dir.strip_prefix(&root_path).ok() else {
        return Ok(true);
    };
    let mut current_path = root_path;
    for component in relative_output_dir.components() {
        current_path.push(component.as_os_str());
        if is_existing_link(&current_path)? {
            return Ok(true);
        }
    }

    Ok(false)
}

fn is_existing_link(path: &Path) -> Result<bool, ConfigError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ConfigError::new(format!(
            "failed to inspect BBDown output directory component {}: {error}",
            path.display()
        ))),
    }
}

fn default_app_base_path() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
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
            "--Cache:TaskStatePath".to_owned(),
            "/tmp/cache-state/tasks.json".to_owned(),
            "--Cache:ServerName".to_owned(),
            "Test Cache".to_owned(),
        ])
        .expect("options should parse");

        assert_eq!("Test Cache", options.server_name);
        assert_eq!(PathBuf::from("/tmp/cache"), options.root_path);
        assert_eq!(
            PathBuf::from("/tmp/cache-state/tasks.json"),
            options.task_state_path
        );
        assert_eq!(
            "127.0.0.1:51000".parse::<SocketAddr>().unwrap(),
            options.grpc_listen_addr().unwrap()
        );
    }

    #[test]
    fn parses_bilibili_worker_and_bbdown_args() {
        let options = CacheServerOptions::from_args([
            "--Cache:RootPath".to_owned(),
            "/tmp/cache".to_owned(),
            "--Cache:BilibiliWorkerEnabled".to_owned(),
            "off".to_owned(),
            "--Cache:BilibiliWorkerMaxConcurrentTasks".to_owned(),
            "2".to_owned(),
            "--Cache:BBDownOutputDir".to_owned(),
            "/tmp/cache/bilibili".to_owned(),
            "--Cache:BBDownArchivePath".to_owned(),
            "/tmp/state/bbdown.json".to_owned(),
            "--Cache:BBDownFfmpegPath".to_owned(),
            "/opt/homebrew/bin/ffmpeg".to_owned(),
        ])
        .expect("options should parse");

        assert!(!options.bilibili_worker_enabled);
        assert_eq!(2, options.bilibili_worker_max_concurrent_tasks);
        assert_eq!(
            normalize_existing_path_prefix(Path::new("/tmp/cache/bilibili")),
            options.bbdown_output_dir()
        );
        assert_eq!(
            PathBuf::from("/tmp/state/bbdown.json"),
            options.bbdown_archive_path()
        );
        assert_eq!(
            PathBuf::from("/opt/homebrew/bin/ffmpeg"),
            options.bbdown_ffmpeg_path
        );
    }

    #[test]
    fn derives_bbdown_paths_from_root_and_task_state_paths() {
        let options = CacheServerOptions {
            root_path: PathBuf::from("/tmp/cache-root"),
            task_state_path: PathBuf::from("/tmp/state/tasks.json"),
            ..CacheServerOptions::default()
        };

        assert_eq!(
            normalize_existing_path_prefix(Path::new("/tmp/cache-root/Bilibili")),
            options.bbdown_output_dir()
        );
        assert_eq!(
            PathBuf::from("/tmp/state/bbdown-archive.json"),
            options.bbdown_archive_path()
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
    fn rejects_zero_bilibili_worker_concurrency() {
        let options = CacheServerOptions {
            bilibili_worker_max_concurrent_tasks: 0,
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[test]
    fn rejects_bbdown_output_dir_outside_cache_root() {
        let options = CacheServerOptions {
            root_path: PathBuf::from("/tmp/cache-root"),
            bbdown_output_dir: Some(PathBuf::from("/tmp/outside-bbdown")),
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[test]
    fn rejects_bbdown_output_dir_that_escapes_cache_root() {
        let options = CacheServerOptions {
            root_path: PathBuf::from("/tmp/cache-root"),
            bbdown_output_dir: Some(PathBuf::from("/tmp/cache-root/../outside-bbdown")),
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[test]
    fn rejects_bbdown_output_dir_with_parent_component_inside_cache_root() {
        let options = CacheServerOptions {
            root_path: PathBuf::from("/tmp/cache-root"),
            bbdown_output_dir: Some(PathBuf::from("/tmp/cache-root/linked-parent/../Bilibili")),
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_default_bbdown_output_dir_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache-root");
        let outside_path = temp.path().join("outside");
        fs::create_dir_all(&root_path).unwrap();
        fs::create_dir_all(&outside_path).unwrap();
        let root_path = root_path.canonicalize().unwrap();
        symlink(&outside_path, root_path.join("Bilibili")).unwrap();
        let options = CacheServerOptions {
            root_path,
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_bbdown_output_dir_with_symlink_component() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("cache-root");
        let outside_path = temp.path().join("outside");
        fs::create_dir_all(&root_path).unwrap();
        fs::create_dir_all(&outside_path).unwrap();
        let root_path = root_path.canonicalize().unwrap();
        symlink(&outside_path, root_path.join("linked-parent")).unwrap();
        let options = CacheServerOptions {
            root_path: root_path.clone(),
            bbdown_output_dir: Some(root_path.join("linked-parent/Bilibili")),
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_normalization_resolves_root_symlink_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_parent = temp.path().join("real-parent");
        let real_root = real_parent.join("cache-root");
        fs::create_dir_all(&real_root).unwrap();
        let link_parent = temp.path().join("link-parent");
        symlink(&real_parent, &link_parent).unwrap();

        let options = CacheServerOptions {
            root_path: link_parent.join("cache-root"),
            ..CacheServerOptions::default()
        }
        .normalized_for_runtime();

        assert_eq!(real_root.canonicalize().unwrap(), options.root_path);
        assert_eq!(
            options.root_path.join("Bilibili"),
            options.bbdown_output_dir()
        );
        options
            .validate()
            .expect("normalized options should validate");
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
