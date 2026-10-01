use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::resolver_catalog::{self, Region};

pub(crate) const MAX_SETTINGS_BYTES: usize = 64 * 1024;
const MAX_CUSTOM_ENTRIES: usize = 32;
const MAX_NAME_BYTES: usize = 128;
const MAX_SETTINGS_FILE_BYTES: usize = MAX_SETTINGS_BYTES;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolverSettings {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) disabled_builtin_host_ids: Vec<String>,
    #[serde(default)]
    pub(crate) custom: Vec<CustomResolver>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustomResolver {
    pub(crate) name: String,
    pub(crate) origin: String,
    pub(crate) regions: Vec<Region>,
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolverCandidate {
    pub(crate) host_id: String,
    pub(crate) origin: String,
    pub(crate) name: String,
    pub(crate) regions: Vec<Region>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolverSettingsSnapshot {
    pub(crate) settings: ResolverSettings,
    pub(crate) revision: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolverSettingsStore {
    path: Arc<PathBuf>,
    state: Arc<Mutex<ResolverSettingsSnapshot>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedResolverSettings {
    version: u32,
    revision: u64,
    #[serde(default)]
    disabled_builtin_host_ids: Vec<String>,
    #[serde(default)]
    custom: Vec<CustomResolver>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ResolverSettingsUpdateError {
    StaleRevision,
    Invalid(String),
    Persistence,
}

impl Default for ResolverSettings {
    fn default() -> Self {
        Self {
            version: 1,
            disabled_builtin_host_ids: Vec::new(),
            custom: Vec::new(),
        }
    }
}

impl ResolverSettings {
    #[cfg(test)]
    pub(crate) fn parse(json: &str) -> Result<Self, String> {
        if json.len() > MAX_SETTINGS_BYTES {
            return Err("resolver settings exceed size limit".to_owned());
        }
        let settings: Self =
            serde_json::from_str(json).map_err(|_| "invalid resolver settings JSON".to_owned())?;
        settings.validate()?;
        Ok(settings)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported resolver settings version".to_owned());
        }
        if self.custom.len() > MAX_CUSTOM_ENTRIES {
            return Err("too many custom resolvers".to_owned());
        }

        let catalog = resolver_catalog::embedded_catalog()?;
        let builtin_ids: HashSet<String> = catalog
            .iter()
            .map(|entry| resolver_catalog::normalize_host_id(&entry.host))
            .collect::<Result<_, _>>()?;
        let mut disabled = HashSet::new();
        for id in &self.disabled_builtin_host_ids {
            let normalized = resolver_catalog::normalize_host_id(id)
                .map_err(|_| "invalid disabled builtin host ID".to_owned())?;
            if !disabled.insert(normalized) {
                return Err("invalid disabled builtin host ID".to_owned());
            }
        }

        let mut all_ids = builtin_ids;
        for custom in &self.custom {
            validate_name(&custom.name)?;
            validate_regions(&custom.regions)?;
            let host_id = validate_origin(&custom.origin)?;
            if !all_ids.insert(host_id) {
                return Err("duplicate resolver host ID".to_owned());
            }
        }
        Ok(())
    }

    pub(crate) fn effective_candidates(&self) -> Result<Vec<ResolverCandidate>, String> {
        self.validate()?;
        let disabled: HashSet<String> = self
            .disabled_builtin_host_ids
            .iter()
            .map(|id| resolver_catalog::normalize_host_id(id))
            .collect::<Result<_, _>>()?;
        let mut candidates = Vec::new();
        for entry in resolver_catalog::embedded_catalog()? {
            let host_id = resolver_catalog::normalize_host_id(&entry.host)?;
            if !disabled.contains(&host_id) {
                candidates.push(ResolverCandidate {
                    origin: format!("https://{}/", entry.host),
                    host_id,
                    name: entry.name,
                    regions: entry.regions,
                });
            }
        }
        for custom in &self.custom {
            if custom.enabled {
                candidates.push(ResolverCandidate {
                    host_id: validate_origin(&custom.origin)?,
                    origin: custom.origin.clone(),
                    name: custom.name.clone(),
                    regions: custom.regions.clone(),
                });
            }
        }
        Ok(candidates)
    }
}

impl ResolverSettingsStore {
    pub(crate) fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let snapshot = match read_persisted_settings(&path)? {
            Some(snapshot) => snapshot,
            None => ResolverSettingsSnapshot {
                settings: ResolverSettings::default(),
                revision: 0,
            },
        };
        Ok(Self {
            path: Arc::new(path),
            state: Arc::new(Mutex::new(snapshot)),
        })
    }

    pub(crate) fn snapshot(&self) -> ResolverSettingsSnapshot {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn effective_candidates(&self) -> Result<Vec<ResolverCandidate>, String> {
        self.snapshot().settings.effective_candidates()
    }

    pub(crate) fn update(
        &self,
        settings: ResolverSettings,
        expected_revision: u64,
    ) -> Result<ResolverSettingsSnapshot, ResolverSettingsUpdateError> {
        settings
            .validate()
            .map_err(ResolverSettingsUpdateError::Invalid)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.revision != expected_revision {
            return Err(ResolverSettingsUpdateError::StaleRevision);
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or(ResolverSettingsUpdateError::Persistence)?;
        let next = ResolverSettingsSnapshot { settings, revision };
        persist_settings(&self.path, &next)
            .map_err(|_| ResolverSettingsUpdateError::Persistence)?;
        *state = next.clone();
        Ok(next)
    }
}

fn read_persisted_settings(path: &Path) -> io::Result<Option<ResolverSettingsSnapshot>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(invalid_settings_file()),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_SETTINGS_FILE_BYTES as u64 {
        return Err(invalid_settings_file());
    }
    let file = nofollow_open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_SETTINGS_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid_settings_file())?;
    if bytes.len() > MAX_SETTINGS_FILE_BYTES {
        return Err(invalid_settings_file());
    }
    let persisted: PersistedResolverSettings =
        serde_json::from_slice(&bytes).map_err(|_| invalid_settings_file())?;
    let settings = ResolverSettings {
        version: persisted.version,
        disabled_builtin_host_ids: persisted.disabled_builtin_host_ids,
        custom: persisted.custom,
    };
    settings.validate().map_err(|_| invalid_settings_file())?;
    Ok(Some(ResolverSettingsSnapshot {
        settings,
        revision: persisted.revision,
    }))
}

fn persist_settings(path: &Path, snapshot: &ResolverSettingsSnapshot) -> io::Result<()> {
    let parent = path.parent().ok_or_else(invalid_settings_file)?;
    fs::create_dir_all(parent).map_err(|_| invalid_settings_file())?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|_| invalid_settings_file())?;
    if !parent_metadata.file_type().is_dir() {
        return Err(invalid_settings_file());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => return Err(invalid_settings_file()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(invalid_settings_file()),
    }
    let persisted = PersistedResolverSettings {
        version: snapshot.settings.version,
        revision: snapshot.revision,
        disabled_builtin_host_ids: snapshot.settings.disabled_builtin_host_ids.clone(),
        custom: snapshot.settings.custom.clone(),
    };
    let bytes = serde_json::to_vec(&persisted).map_err(|_| invalid_settings_file())?;
    if bytes.len() > MAX_SETTINGS_FILE_BYTES {
        return Err(invalid_settings_file());
    }
    let temporary_path = parent.join(format!(".resolver-settings-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut temporary = create_new_private_file(&temporary_path)?;
        temporary
            .write_all(&bytes)
            .map_err(|_| invalid_settings_file())?;
        temporary.sync_all().map_err(|_| invalid_settings_file())?;
        fs::rename(&temporary_path, path).map_err(|_| invalid_settings_file())?;
        if let Ok(directory) = nofollow_open_directory(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn invalid_settings_file() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid resolver settings file")
}

#[cfg(unix)]
fn nofollow_open(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn nofollow_open(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn create_new_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn create_new_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(unix)]
fn nofollow_open_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn nofollow_open_directory(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err("invalid resolver name".to_owned());
    }
    Ok(())
}

fn validate_regions(regions: &[Region]) -> Result<(), String> {
    if regions.is_empty() {
        return Err("resolver regions must not be empty".to_owned());
    }
    let unique: HashSet<Region> = regions.iter().copied().collect();
    if unique.len() != regions.len() {
        return Err("duplicate resolver region".to_owned());
    }
    Ok(())
}

fn validate_origin(origin: &str) -> Result<String, String> {
    let url = Url::parse(origin).map_err(|_| "invalid resolver origin".to_owned())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port == 0)
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid resolver origin".to_owned());
    }
    let Some(Host::Domain(host)) = url.host() else {
        return Err("invalid resolver origin".to_owned());
    };
    Ok(format_host_id(host, url.port()))
}

pub(crate) fn normalize_resolver_host_id(host_id: &str) -> Result<String, String> {
    if host_id.is_empty()
        || host_id != host_id.trim()
        || host_id.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, '@' | '/' | '\\' | '?' | '#')
        })
    {
        return Err("invalid resolver host ID".to_owned());
    }
    let url = Url::parse(&format!("https://{host_id}/"))
        .map_err(|_| "invalid resolver host ID".to_owned())?;
    let Some(Host::Domain(host)) = url.host() else {
        return Err("invalid resolver host ID".to_owned());
    };
    let normalized = format_host_id(host, url.port());
    if normalized != host_id
        || url.port().is_some_and(|port| port == 0)
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid resolver host ID".to_owned());
    }
    Ok(normalized)
}

fn format_host_id(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CustomResolver, ResolverSettings, ResolverSettingsStore, ResolverSettingsUpdateError,
    };
    use crate::resolver_catalog::Region;
    use std::fs;

    #[test]
    fn defaults_enable_all_builtin_entries() {
        let settings = ResolverSettings::default();
        assert_eq!(settings.effective_candidates().unwrap().len(), 43);
    }

    #[test]
    fn disables_builtin_and_preserves_unknown_disabled_ids() {
        let settings = ResolverSettings {
            disabled_builtin_host_ids: vec![
                "api.bili.plus".to_owned(),
                "future.example".to_owned(),
            ],
            ..ResolverSettings::default()
        };
        let candidates = settings.effective_candidates().unwrap();
        assert_eq!(candidates.len(), 42);
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.host_id == "api.bili.plus")
        );
    }

    #[test]
    fn custom_entries_are_appended_when_enabled() {
        let settings = ResolverSettings {
            custom: vec![CustomResolver {
                name: "custom".to_owned(),
                origin: "https://custom.example:8443/".to_owned(),
                regions: vec![Region::Cn],
                enabled: true,
            }],
            ..ResolverSettings::default()
        };
        let candidates = settings.effective_candidates().unwrap();
        assert_eq!(candidates.len(), 44);
        assert_eq!(candidates.last().unwrap().host_id, "custom.example:8443");
        assert_eq!(
            candidates.last().unwrap().origin,
            "https://custom.example:8443/"
        );
    }

    #[test]
    fn custom_https_ports_are_validated_and_canonicalized() {
        for (origin, expected) in [
            ("https://Resolver.Example:8443/", "resolver.example:8443"),
            ("https://resolver.example:443/", "resolver.example"),
        ] {
            let settings = ResolverSettings {
                custom: vec![CustomResolver {
                    name: "custom".to_owned(),
                    origin: origin.to_owned(),
                    regions: vec![Region::Cn],
                    enabled: true,
                }],
                ..ResolverSettings::default()
            };
            assert_eq!(
                settings
                    .effective_candidates()
                    .unwrap()
                    .last()
                    .unwrap()
                    .host_id,
                expected
            );
        }
        for origin in [
            "https://resolver.example:0/",
            "https://resolver.example:65536/",
            "https://resolver.example:8443/path",
        ] {
            let settings = ResolverSettings {
                custom: vec![CustomResolver {
                    name: "custom".to_owned(),
                    origin: origin.to_owned(),
                    regions: vec![Region::Cn],
                    enabled: true,
                }],
                ..ResolverSettings::default()
            };
            assert!(
                settings.effective_candidates().is_err(),
                "accepted {origin}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_builtin_and_custom_ids() {
        let settings = ResolverSettings {
            custom: vec![CustomResolver {
                name: "duplicate".to_owned(),
                origin: "https://API.BILI.PLUS/".to_owned(),
                regions: vec![Region::All],
                enabled: true,
            }],
            ..ResolverSettings::default()
        };
        assert!(settings.effective_candidates().is_err());
    }

    #[test]
    fn secret_url_is_rejected_without_echoing_input() {
        let input = r#"{"version":1,"custom":[{"name":"x","origin":"https://user:secret@example.com/?token=secret","regions":["all"],"enabled":true}]}"#;
        let error = ResolverSettings::parse(input).unwrap_err();
        assert!(!error.contains("secret"));
        for origin in [
            "https://example.com/?token=secret",
            "https://example.com/#secret",
        ] {
            let input = format!(
                r#"{{"version":1,"custom":[{{"name":"x","origin":"{origin}","regions":["all"],"enabled":true}}]}}"#
            );
            let error = ResolverSettings::parse(&input).unwrap_err();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn enforces_size_name_and_custom_count_bounds() {
        assert!(ResolverSettings::parse(&" ".repeat(64 * 1024 + 1)).is_err());
        let mut settings = ResolverSettings {
            custom: (0..33)
                .map(|index| CustomResolver {
                    name: format!("custom-{index}"),
                    origin: format!("https://custom-{index}.example/"),
                    regions: vec![Region::All],
                    enabled: false,
                })
                .collect(),
            ..ResolverSettings::default()
        };
        assert!(settings.effective_candidates().is_err());
        settings.custom = vec![CustomResolver {
            name: "n".repeat(129),
            origin: "https://name.example/".to_owned(),
            regions: vec![Region::All],
            enabled: false,
        }];
        assert!(settings.effective_candidates().is_err());
    }

    #[test]
    fn rejects_unknown_schema_and_version() {
        assert!(ResolverSettings::parse(r#"{"version":2}"#).is_err());
        assert!(ResolverSettings::parse(r#"{"version":1,"token":"hidden"}"#).is_err());
    }

    #[test]
    fn store_defaults_and_persists_settings_and_revision_across_restart() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("resolver-settings.json");
        let store = ResolverSettingsStore::load(&path).expect("store should load defaults");
        assert_eq!(0, store.snapshot().revision);
        assert_eq!(43, store.effective_candidates().unwrap().len());

        let updated = store
            .update(
                ResolverSettings {
                    custom: vec![CustomResolver {
                        name: "custom".to_owned(),
                        origin: "https://resolver.example/".to_owned(),
                        regions: vec![Region::Cn],
                        enabled: true,
                    }],
                    ..ResolverSettings::default()
                },
                0,
            )
            .expect("settings should persist");
        assert_eq!(1, updated.revision);
        drop(store);

        let restarted = ResolverSettingsStore::load(&path).expect("store should reload");
        assert_eq!(updated, restarted.snapshot());
        assert_eq!(44, restarted.effective_candidates().unwrap().len());
        let file = fs::read_to_string(&path).expect("settings file should be readable");
        assert!(file.contains("\"revision\":1"));
    }

    #[test]
    fn stale_revision_is_rejected_after_restart() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("resolver-settings.json");
        let store = ResolverSettingsStore::load(&path).expect("store should load defaults");
        let first = store
            .update(ResolverSettings::default(), 0)
            .expect("initial update should succeed");
        assert_eq!(1, first.revision);
        drop(store);

        let restarted = ResolverSettingsStore::load(&path).expect("store should reload");
        assert_eq!(
            Err(ResolverSettingsUpdateError::StaleRevision),
            restarted.update(ResolverSettings::default(), 0)
        );
    }

    #[test]
    fn malformed_file_is_rejected_without_being_overwritten() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("resolver-settings.json");
        fs::create_dir_all(temp.path()).expect("parent should exist");
        fs::write(&path, b"{ malformed secret=must-not-be-logged")
            .expect("malformed file should be written");
        let error = ResolverSettingsStore::load(&path).unwrap_err();
        assert!(!error.to_string().contains("secret"));
        assert_eq!(
            b"{ malformed secret=must-not-be-logged".as_slice(),
            fs::read(&path)
                .expect("malformed file should remain untouched")
                .as_slice()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_settings_file_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let target = temp.path().join("target.json");
        let link = temp.path().join("resolver-settings.json");
        fs::write(&target, b"{}").expect("target should be written");
        symlink(&target, &link).expect("symlink should be created");
        assert!(ResolverSettingsStore::load(link).is_err());
        assert_eq!(b"{}".as_slice(), fs::read(target).unwrap().as_slice());
    }
}
