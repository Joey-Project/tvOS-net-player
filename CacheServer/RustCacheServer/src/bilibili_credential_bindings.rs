use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const BINDINGS_VERSION: u32 = 1;
const MAX_BINDINGS_FILE_BYTES: usize = 256 * 1024;
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

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

// Preserves complete binding content across cooperating writers, not against same-UID writers
// that ignore or replace the persistent lock file.
struct BindingLock {
    _file: File,
}

impl BindingLock {
    fn acquire(binding_path: &Path) -> Result<Self, BindingError> {
        let lock_path = lock_path(binding_path);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .mode(0o600);
        }
        let lock_file = options
            .open(lock_path)
            .map_err(|_| BindingError::Unavailable)?;
        let metadata = lock_file
            .metadata()
            .map_err(|_| BindingError::Unavailable)?;
        if !metadata.is_file() {
            return Err(BindingError::Unavailable);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o600 != 0o600
                || metadata.mode() & 0o077 != 0
            {
                return Err(BindingError::Unavailable);
            }
        }

        let deadline = Instant::now() + LOCK_WAIT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                return Err(BindingError::Unavailable);
            }
            match lock_file.try_lock() {
                Ok(()) => return Ok(Self { _file: lock_file }),
                Err(TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(BindingError::Unavailable);
                    }
                    thread::sleep(LOCK_RETRY_INTERVAL.min(deadline.duration_since(now)));
                }
                Err(TryLockError::Error(_)) => return Err(BindingError::Unavailable),
            }
        }
    }
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
    let _lock = BindingLock::acquire(&path)?;
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

fn lock_path(binding_path: &Path) -> PathBuf {
    let mut path = binding_path.as_os_str().to_owned();
    path.push(".lock");
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
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
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
    use std::{
        fs,
        path::Path,
        process::{Child, Command, Output, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    // Credential fixture IDs from joey-private-v3: bearer-a and bearer-b.
    const WEB_COOKIE_A: &str = "codex_synth_v1_bearer_a";
    const WEB_COOKIE_B: &str = "JoeyPrivateV3BearerSlotB7Q9M3X5";
    const CHILD_CREDENTIAL_PATH: &str = "BINDINGS_TEST_CREDENTIAL_PATH";
    const CHILD_PROFILE_ID: &str = "BINDINGS_TEST_PROFILE_ID";
    const CHILD_COOKIE_ID: &str = "BINDINGS_TEST_COOKIE_ID";
    const CHILD_ACCOUNT_ID: &str = "BINDINGS_TEST_ACCOUNT_ID";
    const CHILD_START_PATH: &str = "BINDINGS_TEST_START_PATH";
    const CHILD_READY_PATH: &str = "BINDINGS_TEST_READY_PATH";
    const CHILD_EXPECTED_RESULT: &str = "BINDINGS_TEST_EXPECTED_RESULT";
    const CHILD_TIMEOUT: Duration = Duration::from_secs(10);
    const FIFO_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

    struct TestChild {
        child: Option<Child>,
    }

    impl TestChild {
        fn new(child: Child) -> Self {
            Self { child: Some(child) }
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

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

    #[test]
    fn rejects_a_stale_same_profile_compare_and_swap() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = credential_path(&temp);
        replace(&path, "default", WEB_COOKIE_A, 31001, None).expect("write initial binding");
        let stale = lookup_profile(&path, "default")
            .expect("read initial binding")
            .expect("initial binding exists");
        replace(&path, "default", WEB_COOKIE_B, 31002, Some(&stale))
            .expect("replace with current binding");

        assert!(matches!(
            replace(&path, "default", WEB_COOKIE_A, 31003, Some(&stale)),
            Err(BindingError::Changed)
        ));
        assert!(matches!(
            lookup_profile(&path, "default").expect("read final binding"),
            Some(binding) if binding.account_id() == 31002
        ));
    }

    #[test]
    fn process_replace_helper() {
        let Some(path) = std::env::var_os(CHILD_CREDENTIAL_PATH) else {
            return;
        };
        let profile_id = std::env::var(CHILD_PROFILE_ID).expect("child profile ID");
        let cookie = match std::env::var(CHILD_COOKIE_ID).as_deref() {
            Ok("a") => WEB_COOKIE_A,
            Ok("b") => WEB_COOKIE_B,
            _ => panic!("unknown child cookie ID"),
        };
        let account_id = std::env::var(CHILD_ACCOUNT_ID)
            .expect("child account ID")
            .parse::<u64>()
            .expect("numeric child account ID");
        let start_path =
            PathBuf::from(std::env::var_os(CHILD_START_PATH).expect("child start path"));
        let ready_path =
            PathBuf::from(std::env::var_os(CHILD_READY_PATH).expect("child ready path"));
        fs::write(&ready_path, b"ready").expect("signal child ready");

        let deadline = Instant::now() + CHILD_TIMEOUT;
        while !start_path.exists() {
            assert!(Instant::now() < deadline, "child start gate timed out");
            thread::sleep(Duration::from_millis(5));
        }

        let result = replace(Path::new(&path), &profile_id, cookie, account_id, None);
        match std::env::var(CHILD_EXPECTED_RESULT)
            .expect("child expected result")
            .as_str()
        {
            "ok" => assert!(result.is_ok(), "child replacement failed"),
            "unavailable" => assert!(
                matches!(result, Err(BindingError::Unavailable)),
                "child did not report lock contention as unavailable"
            ),
            _ => panic!("unknown expected child result"),
        }
    }

    #[test]
    fn process_fifo_lookup_helper() {
        let Some(path) = std::env::var_os(CHILD_CREDENTIAL_PATH) else {
            return;
        };
        assert!(matches!(
            lookup(Path::new(&path), "default", WEB_COOKIE_A),
            Err(BindingError::Unavailable)
        ));
    }

    fn spawn_replace_process(
        temp: &tempfile::TempDir,
        start_path: &Path,
        profile_id: &str,
        cookie_id: &str,
        account_id: u64,
        expected_result: &str,
    ) -> (TestChild, PathBuf) {
        let ready_path = temp.path().join(format!("{profile_id}.ready"));
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "bilibili_credential_bindings::tests::process_replace_helper",
                "--nocapture",
            ])
            .env(CHILD_CREDENTIAL_PATH, credential_path(temp))
            .env(CHILD_PROFILE_ID, profile_id)
            .env(CHILD_COOKIE_ID, cookie_id)
            .env(CHILD_ACCOUNT_ID, account_id.to_string())
            .env(CHILD_START_PATH, start_path)
            .env(CHILD_READY_PATH, &ready_path)
            .env(CHILD_EXPECTED_RESULT, expected_result)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn binding writer process");
        (TestChild::new(child), ready_path)
    }

    #[cfg(unix)]
    fn spawn_fifo_lookup_process(credentials: &Path) -> TestChild {
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "bilibili_credential_bindings::tests::process_fifo_lookup_helper",
                "--nocapture",
            ])
            .env(CHILD_CREDENTIAL_PATH, credentials)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn FIFO lookup process");
        TestChild::new(child)
    }

    fn release_processes(start_path: &Path, ready_paths: &[PathBuf]) {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        while !ready_paths.iter().all(|path| path.exists()) {
            assert!(Instant::now() < deadline, "child readiness timed out");
            thread::sleep(Duration::from_millis(5));
        }
        fs::write(start_path, b"go").expect("release child start gate");
    }

    fn finish_process(mut child: TestChild, timeout: Duration) -> Output {
        let deadline = Instant::now() + timeout;
        loop {
            let status = child
                .child
                .as_mut()
                .expect("owned child process")
                .try_wait();
            match status {
                Ok(Some(_)) => {
                    return child
                        .child
                        .take()
                        .expect("owned child process")
                        .wait_with_output()
                        .expect("collect child output");
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => panic!("child process timed out after {timeout:?}"),
                Err(error) => panic!("failed to poll child process: {error}"),
            }
        }
    }

    fn assert_process_success(child: TestChild, timeout: Duration) {
        let output = finish_process(child, timeout);
        assert!(
            output.status.success(),
            "child process failed; stdout: {}; stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn concurrent_process_updates_preserve_different_profiles() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let credentials = credential_path(&temp);
        let binding = binding_path(&credentials);
        let start_path = temp.path().join("start");
        let lock = BindingLock::acquire(&binding).expect("hold transaction lock");
        let (first, first_ready) =
            spawn_replace_process(&temp, &start_path, "profile-a", "a", 31001, "ok");
        let (second, second_ready) =
            spawn_replace_process(&temp, &start_path, "profile-b", "b", 31002, "ok");
        release_processes(&start_path, &[first_ready, second_ready]);
        drop(lock);

        assert_process_success(first, CHILD_TIMEOUT);
        assert_process_success(second, CHILD_TIMEOUT);
        assert!(matches!(
            lookup(&credentials, "profile-a", WEB_COOKIE_A).expect("read first binding"),
            BindingLookup::Matches(binding) if binding.account_id() == 31001
        ));
        assert!(matches!(
            lookup(&credentials, "profile-b", WEB_COOKIE_B).expect("read second binding"),
            BindingLookup::Matches(binding) if binding.account_id() == 31002
        ));
        assert!(
            fs::metadata(lock_path(&binding))
                .expect("persistent lock file")
                .is_file()
        );
    }

    #[test]
    fn process_lock_contention_is_bounded_and_unavailable() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let credentials = credential_path(&temp);
        let binding = binding_path(&credentials);
        let start_path = temp.path().join("start");
        let lock = BindingLock::acquire(&binding).expect("hold transaction lock");
        let (child, ready_path) = spawn_replace_process(
            &temp,
            &start_path,
            "blocked-profile",
            "a",
            31001,
            "unavailable",
        );
        release_processes(&start_path, &[ready_path]);
        let started = Instant::now();
        assert_process_success(child, CHILD_TIMEOUT);
        assert!(started.elapsed() < LOCK_WAIT_TIMEOUT + Duration::from_secs(3));
        drop(lock);
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

    #[cfg(unix)]
    #[test]
    fn lookup_rejects_fifo_sidecar_within_deadline() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let credentials = credential_path(&temp);
        create_fifo(&binding_path(&credentials));

        assert_process_success(spawn_fifo_lookup_process(&credentials), FIFO_LOOKUP_TIMEOUT);
    }

    #[cfg(unix)]
    fn create_fifo(path: &Path) {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let path = CString::new(path.as_os_str().as_bytes()).expect("FIFO path");
        assert_eq!(
            0,
            unsafe { libc::mkfifo(path.as_ptr(), 0o600) },
            "create FIFO"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_fifo_and_insecure_lock_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().expect("temporary directory");
        let credentials = credential_path(&temp);
        let binding = binding_path(&credentials);
        let lock = lock_path(&binding);

        let target = temp.path().join("lock-target");
        fs::write(&target, b"").expect("create symlink target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
            .expect("make symlink target private");
        symlink(&target, &lock).expect("create symlink lock");
        assert!(matches!(
            BindingLock::acquire(&binding),
            Err(BindingError::Unavailable)
        ));
        fs::remove_file(&lock).expect("remove symlink lock");

        create_fifo(&lock);
        assert!(matches!(
            BindingLock::acquire(&binding),
            Err(BindingError::Unavailable)
        ));
        fs::remove_file(&lock).expect("remove FIFO lock");

        fs::write(&lock, b"").expect("create insecure lock");
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o640))
            .expect("make lock accessible to group");
        assert!(matches!(
            BindingLock::acquire(&binding),
            Err(BindingError::Unavailable)
        ));
    }
}
