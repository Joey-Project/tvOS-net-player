use std::{
    env, fmt, fs, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use url::Url;

use crate::task_registry::TaskRetentionPolicy;

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
const DEFAULT_HLS_CACHE_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheServerOptions {
    pub server_id: String,
    pub server_name: String,
    pub root_path: PathBuf,
    pub grpc_listen_url: String,
    pub media_listen_url: String,
    pub public_media_base_uri: Option<String>,
    pub bonjour_enabled: bool,
    pub task_state_path: PathBuf,
    pub task_retention_max_terminal_tasks: usize,
    pub task_retention_terminal_age_days: u64,
    pub allowed_extensions: Vec<String>,
    pub allow_library_item_delete: bool,
    pub hls_cache_max_bytes: u64,
    pub hls_cache_high_watermark_percent: u8,
    pub hls_cache_low_watermark_percent: u8,
    pub bilibili_worker_enabled: bool,
    pub bilibili_worker_max_concurrent_tasks: usize,
    pub bbdown_output_dir: Option<PathBuf>,
    pub bbdown_archive_path: Option<PathBuf>,
    pub bbdown_ffmpeg_path: PathBuf,
    pub bbdown_credential_path: Option<PathBuf>,
    pub bbdown_restricted_area: Option<BbdownRestrictedArea>,
    pub bbdown_restricted_area_proxies: Vec<BbdownRestrictedProxy>,
    pub bbdown_restricted_api_proxies: Vec<BbdownRestrictedProxy>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BbdownRestrictedArea {
    Cn,
    Th,
    Hk,
    Tw,
}

#[derive(Clone, PartialEq, Eq)]
pub struct BbdownRestrictedProxy {
    pub area: Option<BbdownRestrictedArea>,
    pub base_url: String,
}

impl fmt::Debug for BbdownRestrictedProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BbdownRestrictedProxy")
            .field("area", &self.area)
            .field("base_url", &redact_config_url_for_error(&self.base_url))
            .finish()
    }
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
            bonjour_enabled: true,
            task_state_path: state_path.join("tasks.json"),
            task_retention_max_terminal_tasks: 200,
            task_retention_terminal_age_days: 30,
            allowed_extensions: vec![".mp4".to_owned(), ".m4v".to_owned(), ".mov".to_owned()],
            allow_library_item_delete: false,
            hls_cache_max_bytes: DEFAULT_HLS_CACHE_MAX_BYTES,
            hls_cache_high_watermark_percent: 90,
            hls_cache_low_watermark_percent: 80,
            bilibili_worker_enabled: true,
            bilibili_worker_max_concurrent_tasks: 1,
            bbdown_output_dir: None,
            bbdown_archive_path: None,
            bbdown_ffmpeg_path: PathBuf::from("ffmpeg"),
            bbdown_credential_path: None,
            bbdown_restricted_area: None,
            bbdown_restricted_area_proxies: Vec::new(),
            bbdown_restricted_api_proxies: Vec::new(),
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
        if self.hls_cache_high_watermark_percent == 0 || self.hls_cache_high_watermark_percent > 100
        {
            return Err(ConfigError::new(
                "HLS cache high watermark percent must be between 1 and 100.",
            ));
        }
        if self.hls_cache_low_watermark_percent >= self.hls_cache_high_watermark_percent {
            return Err(ConfigError::new(
                "HLS cache low watermark percent must be lower than the high watermark percent.",
            ));
        }
        if self.bilibili_worker_enabled || self.bbdown_output_dir.is_some() {
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
        }
        if let Some(path) = self.bbdown_credential_path.as_deref() {
            validate_bbdown_credential_path(path)?;
        }

        Ok(())
    }

    pub fn normalized_for_runtime(mut self) -> Self {
        self.root_path = self.normalized_root_path();
        if self.bbdown_output_dir.is_some() {
            self.bbdown_output_dir = Some(self.bbdown_output_dir());
        }
        if let Some(path) = self.bbdown_credential_path.take() {
            self.bbdown_credential_path = Some(normalized_absolute_path(&path));
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

    pub fn task_retention_policy(&self) -> TaskRetentionPolicy {
        let max_terminal_tasks = (self.task_retention_max_terminal_tasks > 0)
            .then_some(self.task_retention_max_terminal_tasks);
        let max_terminal_task_age = (self.task_retention_terminal_age_days > 0).then(|| {
            Duration::from_secs(
                self.task_retention_terminal_age_days
                    .saturating_mul(SECONDS_PER_DAY),
            )
        });

        TaskRetentionPolicy::new(max_terminal_tasks, max_terminal_task_age)
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
            "Cache:BonjourEnabled" => self.bonjour_enabled = parse_bool(&value)?,
            "Cache:TaskStatePath" => self.task_state_path = PathBuf::from(value),
            "Cache:TaskRetentionMaxTerminalTasks" => {
                self.task_retention_max_terminal_tasks = value.parse().map_err(|_| {
                    ConfigError::new(format!(
                        "invalid integer for --Cache:TaskRetentionMaxTerminalTasks: {value}"
                    ))
                })?;
            }
            "Cache:TaskRetentionTerminalAgeDays" => {
                self.task_retention_terminal_age_days = value.parse().map_err(|_| {
                    ConfigError::new(format!(
                        "invalid integer for --Cache:TaskRetentionTerminalAgeDays: {value}"
                    ))
                })?;
            }
            "Cache:AllowedExtensions" => {
                self.allowed_extensions = value
                    .split(',')
                    .map(str::trim)
                    .filter(|extension| !extension.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
            }
            "Cache:AllowLibraryItemDelete" => self.allow_library_item_delete = parse_bool(&value)?,
            "Cache:HlsCacheMaxBytes" => {
                self.hls_cache_max_bytes = value.parse().map_err(|_| {
                    ConfigError::new(format!(
                        "invalid integer for --Cache:HlsCacheMaxBytes: {value}"
                    ))
                })?;
            }
            "Cache:HlsCacheHighWatermarkPercent" => {
                self.hls_cache_high_watermark_percent = value.parse().map_err(|_| {
                    ConfigError::new(format!(
                        "invalid integer for --Cache:HlsCacheHighWatermarkPercent: {value}"
                    ))
                })?;
            }
            "Cache:HlsCacheLowWatermarkPercent" => {
                self.hls_cache_low_watermark_percent = value.parse().map_err(|_| {
                    ConfigError::new(format!(
                        "invalid integer for --Cache:HlsCacheLowWatermarkPercent: {value}"
                    ))
                })?;
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
            "Cache:BBDownCredentialPath" => {
                self.bbdown_credential_path = Some(PathBuf::from(value));
            }
            "Cache:BBDownRestrictedArea" => {
                self.bbdown_restricted_area = Some(parse_bbdown_restricted_area(&value)?);
            }
            "Cache:BBDownRestrictedAreaProxy" => {
                self.bbdown_restricted_area_proxies = parse_bbdown_restricted_proxy_list(&value)?;
            }
            "Cache:BBDownRestrictedApiProxy" => {
                self.bbdown_restricted_api_proxies = parse_bbdown_restricted_proxy_list(&value)?;
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

fn parse_bool(value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" | "on" => Ok(true),
        "false" | "0" | "no" | "n" | "off" => Ok(false),
        _ => Err(ConfigError::new(format!("invalid boolean value: {value}"))),
    }
}

fn parse_bbdown_restricted_area(value: &str) -> Result<BbdownRestrictedArea, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "cn" => Ok(BbdownRestrictedArea::Cn),
        "th" => Ok(BbdownRestrictedArea::Th),
        "hk" => Ok(BbdownRestrictedArea::Hk),
        "tw" => Ok(BbdownRestrictedArea::Tw),
        other => Err(ConfigError::new(format!(
            "unsupported BBDown restricted area `{other}`; expected cn, th, hk, or tw"
        ))),
    }
}

fn parse_bbdown_restricted_proxy_list(
    value: &str,
) -> Result<Vec<BbdownRestrictedProxy>, ConfigError> {
    value
        .split(',')
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .map(parse_bbdown_restricted_proxy)
        .collect()
}

fn parse_bbdown_restricted_proxy(spec: &str) -> Result<BbdownRestrictedProxy, ConfigError> {
    let (area, base_url) = if let Some((area, base_url)) = parse_area_prefixed_proxy(spec)? {
        (Some(parse_bbdown_restricted_area(area)?), base_url.trim())
    } else {
        (None, spec)
    };
    if base_url.is_empty() {
        return Err(ConfigError::new(
            "BBDown restricted-area proxy URL cannot be empty",
        ));
    }
    let parsed = Url::parse(base_url).map_err(|error| {
        ConfigError::new(format!(
            "failed to parse BBDown restricted-area proxy URL `{}`: {error}",
            redact_config_url_for_error(base_url)
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError::new(format!(
            "BBDown restricted-area proxy URL `{}` must use http or https",
            redact_config_url_for_error(base_url)
        )));
    }

    Ok(BbdownRestrictedProxy {
        area,
        base_url: base_url.to_owned(),
    })
}

fn parse_area_prefixed_proxy(spec: &str) -> Result<Option<(&str, &str)>, ConfigError> {
    if starts_with_url_scheme(spec) {
        return Ok(None);
    }
    let Some((area, base_url)) = spec.split_once('=') else {
        return Ok(None);
    };
    match area.trim().to_ascii_lowercase().as_str() {
        "cn" | "th" | "hk" | "tw" => Ok(Some((area, base_url))),
        other => Err(ConfigError::new(format!(
            "unsupported BBDown restricted area `{other}`; expected cn, th, hk, or tw"
        ))),
    }
}

fn starts_with_url_scheme(value: &str) -> bool {
    let Some(scheme_end) = value.find("://") else {
        return false;
    };
    let scheme = &value[..scheme_end];
    scheme
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn redact_config_url_for_error(raw: &str) -> String {
    Url::parse(raw).map_or_else(
        |_| "<redacted restricted-area proxy URL>".to_owned(),
        |mut url| {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_path("");
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        },
    )
}

#[derive(Deserialize)]
struct BbdownCredentialFile {
    cookie: Option<String>,
    access_key: Option<String>,
    #[serde(default)]
    tv_access_key: Option<String>,
}

fn validate_bbdown_credential_path(path: &Path) -> Result<(), ConfigError> {
    let raw = fs::read_to_string(path).map_err(|error| {
        ConfigError::new(format!(
            "failed to read BBDown credential file {}: {error}",
            path.display()
        ))
    })?;
    let credentials: BbdownCredentialFile = serde_json::from_str(&raw).map_err(|error| {
        ConfigError::new(format!(
            "failed to parse BBDown credential file {}: {error}",
            path.display()
        ))
    })?;
    let _ = (
        credentials.cookie,
        credentials.access_key,
        credentials.tv_access_key,
    );
    Ok(())
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
    pub(crate) fn new(message: impl Into<String>) -> Self {
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
            "--Cache:AllowLibraryItemDelete".to_owned(),
            "true".to_owned(),
        ])
        .expect("options should parse");

        assert_eq!("Test Cache", options.server_name);
        assert!(options.allow_library_item_delete);
        assert!(options.bonjour_enabled);
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
    fn parses_bonjour_disabled_arg() {
        let options = CacheServerOptions::from_args([
            "--Cache:BonjourEnabled".to_owned(),
            "false".to_owned(),
        ])
        .expect("options should parse");

        assert!(!options.bonjour_enabled);
    }

    #[test]
    fn parses_bilibili_worker_and_bbdown_args() {
        let options = CacheServerOptions::from_args([
            "--Cache:RootPath".to_owned(),
            "/tmp/cache".to_owned(),
            "--Cache:HlsCacheMaxBytes".to_owned(),
            "123456".to_owned(),
            "--Cache:HlsCacheHighWatermarkPercent".to_owned(),
            "85".to_owned(),
            "--Cache:HlsCacheLowWatermarkPercent".to_owned(),
            "70".to_owned(),
            "--Cache:BilibiliWorkerEnabled".to_owned(),
            "off".to_owned(),
            "--Cache:BilibiliWorkerMaxConcurrentTasks".to_owned(),
            "2".to_owned(),
            "--Cache:TaskRetentionMaxTerminalTasks".to_owned(),
            "25".to_owned(),
            "--Cache:TaskRetentionTerminalAgeDays".to_owned(),
            "7".to_owned(),
            "--Cache:BBDownOutputDir".to_owned(),
            "/tmp/cache/bilibili".to_owned(),
            "--Cache:BBDownArchivePath".to_owned(),
            "/tmp/state/bbdown.json".to_owned(),
            "--Cache:BBDownFfmpegPath".to_owned(),
            "/opt/homebrew/bin/ffmpeg".to_owned(),
            "--Cache:BBDownRestrictedArea".to_owned(),
            "hk".to_owned(),
            "--Cache:BBDownRestrictedAreaProxy".to_owned(),
            "hk=https://play.example/proxy,https://generic.example/proxy".to_owned(),
            "--Cache:BBDownRestrictedApiProxy".to_owned(),
            "tw=https://api.example/proxy".to_owned(),
        ])
        .expect("options should parse");

        assert!(!options.bilibili_worker_enabled);
        assert_eq!(123456, options.hls_cache_max_bytes);
        assert_eq!(85, options.hls_cache_high_watermark_percent);
        assert_eq!(70, options.hls_cache_low_watermark_percent);
        assert_eq!(2, options.bilibili_worker_max_concurrent_tasks);
        assert_eq!(25, options.task_retention_max_terminal_tasks);
        assert_eq!(7, options.task_retention_terminal_age_days);
        assert_eq!(
            TaskRetentionPolicy::new(Some(25), Some(Duration::from_secs(7 * SECONDS_PER_DAY))),
            options.task_retention_policy()
        );
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
        assert_eq!(
            Some(BbdownRestrictedArea::Hk),
            options.bbdown_restricted_area
        );
        assert_eq!(
            vec![
                BbdownRestrictedProxy {
                    area: Some(BbdownRestrictedArea::Hk),
                    base_url: "https://play.example/proxy".to_owned(),
                },
                BbdownRestrictedProxy {
                    area: None,
                    base_url: "https://generic.example/proxy".to_owned(),
                },
            ],
            options.bbdown_restricted_area_proxies
        );
        assert_eq!(
            vec![BbdownRestrictedProxy {
                area: Some(BbdownRestrictedArea::Tw),
                base_url: "https://api.example/proxy".to_owned(),
            }],
            options.bbdown_restricted_api_proxies
        );
    }

    #[test]
    fn parses_bbdown_credential_path() {
        let temp = tempfile::tempdir().unwrap();
        let credentials_path = temp.path().join("credentials.json");
        fs::write(
            &credentials_path,
            r#"{"cookie":"SESSDATA=secret","access_key":"access-token","tv_access_key":"tv-token"}"#,
        )
        .unwrap();

        let options = CacheServerOptions::from_args([
            "--Cache:BBDownCredentialPath".to_owned(),
            credentials_path.display().to_string(),
        ])
        .expect("options should parse");

        assert_eq!(Some(credentials_path), options.bbdown_credential_path);
    }

    #[test]
    fn rejects_invalid_bbdown_credential_file() {
        let temp = tempfile::tempdir().unwrap();
        let credentials_path = temp.path().join("credentials.json");
        fs::write(&credentials_path, "not json").unwrap();

        let result = CacheServerOptions::from_args([
            "--Cache:BBDownCredentialPath".to_owned(),
            credentials_path.display().to_string(),
        ]);

        assert!(matches!(result, Err(ConfigError { .. })));
    }

    #[test]
    fn rejects_invalid_bbdown_restricted_area() {
        let result = CacheServerOptions::from_args([
            "--Cache:BBDownRestrictedArea".to_owned(),
            "us".to_owned(),
        ]);

        assert!(matches!(
            result,
            Err(ConfigError { message }) if message.contains("expected cn, th, hk, or tw")
        ));
    }

    #[test]
    fn rejects_invalid_bbdown_restricted_proxy_without_leaking_secret_url_parts() {
        let result = CacheServerOptions::from_args([
            "--Cache:BBDownRestrictedAreaProxy".to_owned(),
            "hk=ftp://user:password@example.invalid/private/path?token=secret".to_owned(),
        ]);

        assert!(matches!(
            result,
            Err(ConfigError { message })
                if message.contains("ftp://example.invalid/")
                    && !message.contains("password")
                    && !message.contains("private")
                    && !message.contains("secret")
        ));
    }

    #[test]
    fn zero_task_retention_args_disable_individual_limits() {
        let options = CacheServerOptions::from_args([
            "--Cache:TaskRetentionMaxTerminalTasks".to_owned(),
            "0".to_owned(),
            "--Cache:TaskRetentionTerminalAgeDays".to_owned(),
            "0".to_owned(),
        ])
        .expect("options should parse");

        assert_eq!(
            TaskRetentionPolicy::new(None, None),
            options.task_retention_policy()
        );
    }

    #[test]
    fn default_hls_cache_quota_uses_watermarks() {
        let options = CacheServerOptions::default();

        assert_eq!(50 * 1024 * 1024 * 1024, options.hls_cache_max_bytes);
        assert_eq!(90, options.hls_cache_high_watermark_percent);
        assert_eq!(80, options.hls_cache_low_watermark_percent);
    }

    #[test]
    fn zero_hls_cache_quota_disables_eviction_but_keeps_watermark_validation() {
        let options = CacheServerOptions {
            hls_cache_max_bytes: 0,
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_ok());
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
    fn rejects_invalid_hls_cache_watermarks() {
        let zero_high = CacheServerOptions {
            hls_cache_high_watermark_percent: 0,
            ..CacheServerOptions::default()
        };
        let equal_watermarks = CacheServerOptions {
            hls_cache_high_watermark_percent: 80,
            hls_cache_low_watermark_percent: 80,
            ..CacheServerOptions::default()
        };
        let inverted_watermarks = CacheServerOptions {
            hls_cache_high_watermark_percent: 70,
            hls_cache_low_watermark_percent: 80,
            ..CacheServerOptions::default()
        };

        assert!(zero_high.validate().is_err());
        assert!(equal_watermarks.validate().is_err());
        assert!(inverted_watermarks.validate().is_err());
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
    fn allows_disabled_worker_with_default_bbdown_output_dir_symlink() {
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
            bilibili_worker_enabled: false,
            ..CacheServerOptions::default()
        };

        assert!(options.validate().is_ok());
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
