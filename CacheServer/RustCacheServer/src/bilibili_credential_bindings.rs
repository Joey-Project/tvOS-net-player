use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const BINDINGS_VERSION: u32 = 1;
const MAX_BINDINGS_FILE_BYTES: usize = 256 * 1024;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct VerifiedWebBinding {
    cookie_fingerprint: String,
    account_id: u64,
}

impl VerifiedWebBinding {
    pub(crate) fn account_id(&self) -> u64 {
        self.account_id
    }
}

pub(crate) enum BindingLookup {
    Missing,
    Matches(VerifiedWebBinding),
    FingerprintMismatch(VerifiedWebBinding),
}

#[derive(Debug)]
pub(crate) enum BindingError {
    Unavailable,
    Changed,
}

#[derive(Default, Serialize, Deserialize)]
struct BindingFile {
    version: u32,
    #[serde(default)]
    profiles: BTreeMap<String, VerifiedWebBinding>,
}

pub(crate) fn lookup(
    credential_path: &Path,
    profile_id: &str,
    cookie: &str,
) -> Result<BindingLookup, BindingError> {
    let path = binding_path(credential_path);
    let file = read_file(&path)?;
    let Some(binding) = file.profiles.get(profile_id).cloned() else {
        return Ok(BindingLookup::Missing);
    };
    if binding.cookie_fingerprint == fingerprint(cookie) {
        Ok(BindingLookup::Matches(binding))
    } else {
        Ok(BindingLookup::FingerprintMismatch(binding))
    }
}

pub(crate) fn lookup_profile(
    credential_path: &Path,
    profile_id: &str,
) -> Result<Option<VerifiedWebBinding>, BindingError> {
    Ok(read_file(&binding_path(credential_path))?
        .profiles
        .get(profile_id)
        .cloned())
}

pub(crate) fn replace(
    credential_path: &Path,
    profile_id: &str,
    cookie: &str,
    account_id: u64,
    expected: Option<&VerifiedWebBinding>,
) -> Result<VerifiedWebBinding, BindingError> {
    let path = binding_path(credential_path);
    let mut file = read_file(&path)?;
    if file.profiles.get(profile_id) != expected {
        return Err(BindingError::Changed);
    }

    let binding = VerifiedWebBinding {
        cookie_fingerprint: fingerprint(cookie),
        account_id,
    };
    file.version = BINDINGS_VERSION;
    file.profiles.insert(profile_id.to_owned(), binding.clone());
    write_file(&path, &file)?;
    Ok(binding)
}

fn binding_path(credential_path: &Path) -> PathBuf {
    let mut path = credential_path.as_os_str().to_owned();
    path.push(".bilibili-bindings.json");
    PathBuf::from(path)
}

fn fingerprint(cookie: &str) -> String {
    let digest = Sha256::digest(cookie.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn read_file(path: &Path) -> Result<BindingFile, BindingError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut input = match options.open(path) {
        Ok(input) => input,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BindingFile::default());
        }
        Err(_) => return Err(BindingError::Unavailable),
    };
    let metadata = input.metadata().map_err(|_| BindingError::Unavailable)?;
    if !metadata.is_file() {
        return Err(BindingError::Unavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(BindingError::Unavailable);
        }
    }
    let mut contents = Vec::new();
    std::io::Read::by_ref(&mut input)
        .take((MAX_BINDINGS_FILE_BYTES + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(|_| BindingError::Unavailable)?;
    if contents.len() > MAX_BINDINGS_FILE_BYTES {
        return Err(BindingError::Unavailable);
    }
    let file: BindingFile =
        serde_json::from_slice(&contents).map_err(|_| BindingError::Unavailable)?;
    if file.version != BINDINGS_VERSION {
        return Err(BindingError::Unavailable);
    }
    Ok(file)
}

fn write_file(path: &Path, file: &BindingFile) -> Result<(), BindingError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(BindingError::Unavailable);
    }
    let bytes = serde_json::to_vec(file).map_err(|_| BindingError::Unavailable)?;
    if bytes.len() > MAX_BINDINGS_FILE_BYTES {
        return Err(BindingError::Unavailable);
    }
    let temporary = parent.join(format!(".bilibili-bindings-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(&temporary)
        .map_err(|_| BindingError::Unavailable)?;
    let result = output
        .write_all(&bytes)
        .and_then(|()| output.sync_all())
        .and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(BindingError::Unavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    // Credential fixture IDs from joey-private-v3: bearer-a and bearer-b.
    const WEB_COOKIE_A: &str = "codex_synth_v1_bearer_a";
    const WEB_COOKIE_B: &str = "JoeyPrivateV3BearerSlotB7Q9M3X5";

    fn credential_path(temp: &tempfile::TempDir) -> PathBuf {
        temp.path().join("credentials.json")
    }

    #[test]
    fn stores_only_cookie_fingerprint_and_verified_account_id() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = credential_path(&temp);
        replace(&path, "default", WEB_COOKIE_A, 31001, None).expect("write binding");

        assert!(matches!(
            lookup(&path, "default", WEB_COOKIE_A).expect("lookup"),
            BindingLookup::Matches(binding) if binding.account_id() == 31001
        ));
        assert!(matches!(
            lookup(&path, "default", WEB_COOKIE_B).expect("mismatch lookup"),
            BindingLookup::FingerprintMismatch(_)
        ));
        let contents = fs::read_to_string(binding_path(&path)).expect("read private binding");
        assert!(!contents.contains(WEB_COOKIE_A));
        assert!(contents.contains(&fingerprint(WEB_COOKIE_A)));
    }

    #[test]
    fn distinguishes_missing_from_unreadable_and_stale_bindings() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = credential_path(&temp);
        assert!(matches!(
            lookup(&path, "default", WEB_COOKIE_A).expect("missing binding"),
            BindingLookup::Missing
        ));

        replace(&path, "default", WEB_COOKIE_A, 31001, None).expect("write binding");
        assert!(matches!(
            replace(&path, "default", WEB_COOKIE_A, 31002, None),
            Err(BindingError::Changed)
        ));
        fs::write(binding_path(&path), b"not-json").expect("corrupt binding file");
        assert!(matches!(
            lookup(&path, "default", WEB_COOKIE_A),
            Err(BindingError::Unavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn requires_private_binding_permissions_and_caps_reads() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().expect("temporary directory");
        let path = credential_path(&temp);
        replace(&path, "default", WEB_COOKIE_A, 31001, None).expect("write binding");
        let sidecar = binding_path(&path);
        assert_eq!(
            0o600,
            fs::metadata(&sidecar).expect("private sidecar").mode() & 0o777
        );

        let mut permissions = fs::metadata(&sidecar)
            .expect("sidecar metadata")
            .permissions();
        permissions.set_mode(0o640);
        fs::set_permissions(&sidecar, permissions).expect("make binding too permissive");
        assert!(matches!(
            lookup(&path, "default", WEB_COOKIE_A),
            Err(BindingError::Unavailable)
        ));

        fs::write(&sidecar, vec![b' '; MAX_BINDINGS_FILE_BYTES + 1])
            .expect("write oversized sidecar");
        let mut permissions = fs::metadata(&sidecar)
            .expect("oversized sidecar metadata")
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&sidecar, permissions).expect("restore private mode");
        assert!(matches!(
            lookup(&path, "default", WEB_COOKIE_A),
            Err(BindingError::Unavailable)
        ));
    }
}
