use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bbdown_core::{
    BiliClient, ClientConfig, CredentialProfileSecrets, CredentialRefreshSecret, CredentialStore,
    Credentials, QrLoginCredentialsState,
};
use tokio::time::{sleep, timeout};
use tonic::Status;

use crate::{
    generated::tvos_net_player::v1::{BilibiliLoginSession, BilibiliLoginSessionState},
    task_registry::current_timestamp,
};

const MAX_LOGIN_SESSIONS: usize = 64;
const SESSION_TTL: Duration = Duration::from_secs(180);
const TERMINAL_SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_QR_URL_BYTES: usize = 4096;

type ProviderFuture<T> = Pin<Box<dyn Future<Output = Result<T, ()>> + Send>>;

#[derive(Clone)]
struct QrTicket {
    verification_uri: String,
    key: String,
}

#[derive(Clone)]
enum PollResult {
    Waiting,
    Confirming,
    Expired,
    Succeeded {
        credentials: Credentials,
        refresh_token: Option<String>,
    },
}

trait WebQrProvider: Send + Sync {
    fn create(&self) -> ProviderFuture<QrTicket>;
    fn poll(&self, key: String) -> ProviderFuture<PollResult>;
}

struct BbdownWebQrProvider;

impl WebQrProvider for BbdownWebQrProvider {
    fn create(&self) -> ProviderFuture<QrTicket> {
        Box::pin(async {
            let client =
                BiliClient::new(ClientConfig::default().with_request_timeout(PROVIDER_TIMEOUT));
            let ticket = client.create_web_qr_login().await.map_err(|_| ())?;
            Ok(QrTicket {
                verification_uri: ticket.url,
                key: ticket.key,
            })
        })
    }

    fn poll(&self, key: String) -> ProviderFuture<PollResult> {
        Box::pin(async move {
            let client =
                BiliClient::new(ClientConfig::default().with_request_timeout(PROVIDER_TIMEOUT));
            let result = client
                .poll_web_qr_login_credentials(&key)
                .await
                .map_err(|_| ())?;
            Ok(match result {
                QrLoginCredentialsState::WaitingForScan => PollResult::Waiting,
                QrLoginCredentialsState::WaitingForConfirm => PollResult::Confirming,
                QrLoginCredentialsState::Expired => PollResult::Expired,
                QrLoginCredentialsState::Succeeded { credentials } => PollResult::Succeeded {
                    credentials: credentials.credentials,
                    refresh_token: credentials.refresh_token,
                },
            })
        })
    }
}

#[derive(Clone)]
struct SessionRecord {
    public: BilibiliLoginSession,
    ticket_key: String,
    deadline: Instant,
    finishing: bool,
    completed_at: Option<Instant>,
}

#[derive(Default)]
struct LoginState {
    sessions: HashMap<String, SessionRecord>,
    active_profiles: HashMap<String, String>,
}

struct ProfileReservationGuard {
    manager: BilibiliLoginManager,
    profile_id: String,
    session_id: String,
    armed: bool,
}

impl ProfileReservationGuard {
    fn new(manager: BilibiliLoginManager, profile_id: String, session_id: String) -> Self {
        Self {
            manager,
            profile_id,
            session_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProfileReservationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.manager
                .release_profile(&self.profile_id, &self.session_id);
        }
    }
}

#[derive(Clone)]
pub(crate) struct BilibiliLoginManager {
    state: Arc<Mutex<LoginState>>,
    provider: Arc<dyn WebQrProvider>,
}

impl Default for BilibiliLoginManager {
    fn default() -> Self {
        Self::new(Arc::new(BbdownWebQrProvider))
    }
}

impl BilibiliLoginManager {
    fn new(provider: Arc<dyn WebQrProvider>) -> Self {
        Self {
            state: Arc::new(Mutex::new(LoginState::default())),
            provider,
        }
    }

    pub(crate) async fn start(
        &self,
        profile_id: String,
        credential_path: Option<PathBuf>,
    ) -> Result<BilibiliLoginSession, Status> {
        let path = credential_path.ok_or_else(|| {
            Status::failed_precondition(
                "Configure BBDown credential storage on the server before starting login.",
            )
        })?;
        let session_id = uuid::Uuid::new_v4().to_string();

        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?;
            expire_sessions(&mut state, Instant::now());
            prune_terminal_sessions(&mut state, Instant::now());
            let unmaterialized = state
                .active_profiles
                .values()
                .filter(|active_id| !state.sessions.contains_key(*active_id))
                .count();
            while state.sessions.len() + unmaterialized >= MAX_LOGIN_SESSIONS
                && evict_oldest_terminal_session(&mut state)
            {}
            if state.sessions.len() + unmaterialized >= MAX_LOGIN_SESSIONS {
                return Err(Status::resource_exhausted(
                    "Too many Bilibili login sessions are active.",
                ));
            }
            if state.active_profiles.contains_key(&profile_id) {
                return Err(Status::failed_precondition(
                    "A Bilibili login is already active for this profile.",
                ));
            }
            state
                .active_profiles
                .insert(profile_id.clone(), session_id.clone());
        }
        let mut reservation =
            ProfileReservationGuard::new(self.clone(), profile_id.clone(), session_id.clone());

        let existing = load_profile(&path, &profile_id)?;
        if existing
            .cookie
            .as_deref()
            .is_some_and(|cookie| !cookie.trim().is_empty())
        {
            return Err(Status::failed_precondition(
                "This profile already has a Web cookie; login will not replace it.",
            ));
        }

        let ticket = match timeout(PROVIDER_TIMEOUT, self.provider.create()).await {
            Ok(Ok(ticket)) if valid_qr_uri(&ticket.verification_uri) && !ticket.key.is_empty() => {
                ticket
            }
            _ => {
                return Err(Status::unavailable(
                    "Could not start Bilibili Web QR login. Please retry later.",
                ));
            }
        };

        let now = Instant::now();
        let session = BilibiliLoginSession {
            id: session_id.clone(),
            profile_id,
            method: crate::generated::tvos_net_player::v1::BilibiliLoginMethod::WebQr.into(),
            state: BilibiliLoginSessionState::Pending.into(),
            message: "Waiting for QR scan.".to_owned(),
            verification_uri: ticket.verification_uri,
            created_at: Some(current_timestamp()),
            expires_at: Some(timestamp_after(SESSION_TTL)),
        };
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?;
            expire_sessions(&mut state, now);
            state.sessions.insert(
                session.id.clone(),
                SessionRecord {
                    public: session.clone(),
                    ticket_key: ticket.key,
                    deadline: now + SESSION_TTL,
                    finishing: false,
                    completed_at: None,
                },
            );
        }

        let manager = self.clone();
        let session_id = session.id.clone();
        tokio::spawn(async move { manager.run_session(session_id, path).await });
        reservation.disarm();
        Ok(session)
    }

    pub(crate) fn get(&self, session_id: &str) -> Result<BilibiliLoginSession, Status> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?;
        let now = Instant::now();
        expire_sessions(&mut state, now);
        prune_terminal_sessions(&mut state, now);
        state
            .sessions
            .get(session_id)
            .map(|record| record.public.clone())
            .ok_or_else(|| Status::not_found("Bilibili login session not found."))
    }

    async fn run_session(&self, session_id: String, path: PathBuf) {
        loop {
            let (key, profile_id) = {
                let mut state = match self.state.lock() {
                    Ok(state) => state,
                    Err(_) => return,
                };
                expire_sessions(&mut state, Instant::now());
                let Some(record) = state.sessions.get_mut(&session_id) else {
                    return;
                };
                if record.public.state() != BilibiliLoginSessionState::Pending || record.finishing {
                    return;
                }
                (record.ticket_key.clone(), record.public.profile_id.clone())
            };

            match timeout(PROVIDER_TIMEOUT, self.provider.poll(key)).await {
                Ok(Ok(PollResult::Waiting)) => {}
                Ok(Ok(PollResult::Confirming)) => self.set_state(
                    &session_id,
                    BilibiliLoginSessionState::Pending,
                    "QR scanned; waiting for confirmation.",
                ),
                Ok(Ok(PollResult::Expired)) => {
                    self.finish_without_store(
                        &session_id,
                        &profile_id,
                        BilibiliLoginSessionState::Expired,
                        "The QR login session expired.",
                    );
                    return;
                }
                Ok(Ok(PollResult::Succeeded {
                    credentials,
                    refresh_token,
                })) => {
                    if self
                        .persist_credentials(
                            &session_id,
                            path,
                            profile_id.clone(),
                            credentials,
                            refresh_token,
                        )
                        .await
                    {
                        self.finish_without_store(
                            &session_id,
                            &profile_id,
                            BilibiliLoginSessionState::Ready,
                            "Bilibili Web login completed.",
                        );
                    } else {
                        self.finish_without_store(
                            &session_id,
                            &profile_id,
                            BilibiliLoginSessionState::Error,
                            "Login could not be saved. Check server credential storage and retry.",
                        );
                    }
                    return;
                }
                _ => {
                    self.finish_without_store(
                        &session_id,
                        &profile_id,
                        BilibiliLoginSessionState::Error,
                        "Bilibili Web login could not be completed. Start a new session to retry.",
                    );
                    return;
                }
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn persist_credentials(
        &self,
        session_id: &str,
        path: PathBuf,
        profile_id: String,
        credentials: Credentials,
        refresh_token: Option<String>,
    ) -> bool {
        let cookie = credentials
            .cookie
            .filter(|cookie| !cookie.trim().is_empty());
        let Some(cookie) = cookie else {
            return false;
        };

        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return false,
            };
            if state.active_profiles.get(&profile_id).map(String::as_str) != Some(session_id) {
                return false;
            }
            let Some(record) = state.sessions.get_mut(session_id) else {
                return false;
            };
            if record.public.profile_id != profile_id
                || record.public.state() != BilibiliLoginSessionState::Pending
                || record.finishing
                || Instant::now() >= record.deadline
            {
                return false;
            }
            record.finishing = true;
        }

        let store_profile_id = profile_id.clone();
        let saved = tokio::task::spawn_blocking(move || {
            CredentialStore::new(path).update_profiles(|profiles| {
                let mut current = profiles.profile(&store_profile_id)?;
                if current
                    .cookie
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    return Err(bbdown_core::Error::InvalidInput(
                        "profile already contains a Web cookie".to_owned(),
                    ));
                }
                current.cookie = Some(cookie);
                profiles.set_profile(&store_profile_id, current)?;

                let mut secrets: CredentialProfileSecrets =
                    profiles.profile_secrets(&store_profile_id)?;
                // Core normalization drops cookie refresh secrets without a cookie; carrying one
                // across a new QR login could attach the prior account's token to another user.
                let cookie_secret = refresh_token
                    .filter(|value| !value.trim().is_empty())
                    .map_or_else(CredentialRefreshSecret::default, |value| {
                        CredentialRefreshSecret::default().with_refresh_token(value)
                    });
                secrets.set_cookie(cookie_secret);
                profiles.set_profile_secrets(&store_profile_id, secrets)
            })
        })
        .await
        .is_ok_and(|result| result.is_ok());

        self.complete_claimed_session(session_id, &profile_id, saved) && saved
    }

    fn complete_claimed_session(&self, session_id: &str, profile_id: &str, saved: bool) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let still_owner =
            state.active_profiles.get(profile_id).map(String::as_str) == Some(session_id);
        let still_finishing = state.sessions.get(session_id).is_some_and(|record| {
            record.public.profile_id == profile_id
                && record.public.state() == BilibiliLoginSessionState::Pending
                && record.finishing
        });
        if !still_owner || !still_finishing {
            return false;
        }
        if let Some(record) = state.sessions.get_mut(session_id) {
            record.public.state = if saved {
                BilibiliLoginSessionState::Ready.into()
            } else {
                BilibiliLoginSessionState::Error.into()
            };
            record.public.message = if saved {
                "Bilibili Web login completed.".to_owned()
            } else {
                "Login could not be saved. Check server credential storage and retry.".to_owned()
            };
            record.public.verification_uri.clear();
            record.ticket_key.clear();
            record.finishing = false;
            record.completed_at = Some(Instant::now());
        }
        self.release_profile_locked(&mut state, profile_id, session_id);
        true
    }

    fn set_state(&self, session_id: &str, state_value: BilibiliLoginSessionState, message: &str) {
        if let Ok(mut state) = self.state.lock()
            && let Some(record) = state.sessions.get_mut(session_id)
        {
            record.public.state = state_value.into();
            record.public.message = message.to_owned();
        }
    }

    fn finish_without_store(
        &self,
        session_id: &str,
        profile_id: &str,
        state_value: BilibiliLoginSessionState,
        message: &str,
    ) {
        if let Ok(mut state) = self.state.lock() {
            expire_sessions(&mut state, Instant::now());
            let owns_profile =
                state.active_profiles.get(profile_id).map(String::as_str) == Some(session_id);
            if let Some(record) = state.sessions.get_mut(session_id)
                && owns_profile
                && record.public.state() == BilibiliLoginSessionState::Pending
                && !record.finishing
            {
                record.public.state = state_value.into();
                record.public.message = message.to_owned();
                record.public.verification_uri.clear();
                record.ticket_key.clear();
                record.completed_at = Some(Instant::now());
                self.release_profile_locked(&mut state, profile_id, session_id);
            }
        }
    }

    fn release_profile(&self, profile_id: &str, session_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            self.release_profile_locked(&mut state, profile_id, session_id);
        }
    }

    fn release_profile_locked(&self, state: &mut LoginState, profile_id: &str, session_id: &str) {
        if state.active_profiles.get(profile_id).map(String::as_str) == Some(session_id) {
            state.active_profiles.remove(profile_id);
        }
    }
}

fn load_profile(path: &Path, profile_id: &str) -> Result<Credentials, Status> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(Status::failed_precondition(
            "BBDown credential storage directory is unavailable or not writable.",
        ));
    }
    CredentialStore::new(path.to_path_buf())
        .load_profiles()
        .and_then(|profiles| profiles.profile(profile_id))
        .map_err(|_| {
            Status::failed_precondition(
                "Could not read BBDown credential storage. Check server configuration.",
            )
        })
}

pub(crate) fn credential_login_available(path: Option<&Path>, profile: Option<&str>) -> bool {
    let Some(path) = path else {
        return false;
    };
    if !credential_store_parent_is_writable(path) {
        return false;
    }
    if !path.exists() {
        return true;
    }
    let Ok(profiles) = CredentialStore::new(path.to_path_buf()).load_profiles() else {
        return false;
    };
    profile.is_none_or(|profile| profiles.profile(profile).is_ok())
}

fn credential_store_parent_is_writable(path: &Path) -> bool {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() || path.is_dir() {
        return false;
    }

    let probe_path = parent.join(format!(".bbdown-write-check-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let Ok(mut probe) = options.open(&probe_path) else {
        return false;
    };
    let write_succeeded = probe.write_all(&[0]).is_ok() && probe.sync_all().is_ok();
    drop(probe);
    let cleanup_succeeded = fs::remove_file(&probe_path).is_ok();
    write_succeeded && cleanup_succeeded
}

fn prune_terminal_sessions(state: &mut LoginState, now: Instant) {
    state.sessions.retain(|_, record| {
        record.public.state() == BilibiliLoginSessionState::Pending
            || record.finishing
            || record
                .completed_at
                .is_none_or(|completed_at| now.duration_since(completed_at) < TERMINAL_SESSION_TTL)
    });
}

fn evict_oldest_terminal_session(state: &mut LoginState) -> bool {
    let oldest = state
        .sessions
        .iter()
        .filter_map(|(id, record)| {
            record
                .completed_at
                .map(|completed_at| (id.clone(), completed_at))
        })
        .min_by_key(|(_, completed_at)| *completed_at)
        .map(|(id, _)| id);
    oldest.is_some_and(|id| state.sessions.remove(&id).is_some())
}

fn valid_qr_uri(uri: &str) -> bool {
    if uri.len() > MAX_QR_URL_BYTES || uri.chars().any(char::is_control) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(uri) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed
            .host_str()
            .is_some_and(|host| host == "bilibili.com" || host.ends_with(".bilibili.com"))
}

fn expire_sessions(state: &mut LoginState, now: Instant) {
    let mut released = Vec::new();
    for record in state.sessions.values_mut() {
        if record.public.state() == BilibiliLoginSessionState::Pending
            && !record.finishing
            && now >= record.deadline
        {
            record.public.state = BilibiliLoginSessionState::Expired.into();
            record.public.message = "The QR login session expired.".to_owned();
            record.public.verification_uri.clear();
            record.ticket_key.clear();
            record.completed_at = Some(now);
            released.push((record.public.profile_id.clone(), record.public.id.clone()));
        }
    }
    for (profile_id, session_id) in released {
        if state.active_profiles.get(&profile_id).map(String::as_str) == Some(&session_id) {
            state.active_profiles.remove(&profile_id);
        }
    }
}

fn timestamp_after(duration: Duration) -> prost_types::Timestamp {
    let expiry = SystemTime::now() + duration;
    let since_epoch = expiry.duration_since(UNIX_EPOCH).unwrap_or_default();
    prost_types::Timestamp {
        seconds: i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
        nanos: since_epoch.subsec_nanos() as i32,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use bbdown_core::{
        CredentialProfileSecrets, CredentialRefreshSecret, CredentialStore, Credentials,
    };
    use tonic::Code;

    use super::*;

    struct FakeProvider {
        polls: AtomicUsize,
        result: PollResult,
    }

    #[derive(Default)]
    struct PausedCreateProvider {
        started: Arc<tokio::sync::Notify>,
        resume: Arc<tokio::sync::Notify>,
        calls: AtomicUsize,
    }

    impl WebQrProvider for PausedCreateProvider {
        fn create(&self) -> ProviderFuture<QrTicket> {
            let started = Arc::clone(&self.started);
            let resume = Arc::clone(&self.resume);
            let first_call = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                started.notify_one();
                if first_call {
                    resume.notified().await;
                }
                Ok(QrTicket {
                    verification_uri: "https://passport.bilibili.com/qr/mock".to_owned(),
                    key: "private-ticket-key".to_owned(),
                })
            })
        }

        fn poll(&self, _key: String) -> ProviderFuture<PollResult> {
            Box::pin(async { Ok(PollResult::Waiting) })
        }
    }

    impl WebQrProvider for FakeProvider {
        fn create(&self) -> ProviderFuture<QrTicket> {
            Box::pin(async {
                Ok(QrTicket {
                    verification_uri: "https://passport.bilibili.com/qr/mock".to_owned(),
                    key: "private-ticket-key".to_owned(),
                })
            })
        }

        fn poll(&self, _key: String) -> ProviderFuture<PollResult> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            let result = self.result.clone();
            Box::pin(async move { Ok(result) })
        }
    }

    fn manager(result: PollResult) -> BilibiliLoginManager {
        BilibiliLoginManager::new(Arc::new(FakeProvider {
            polls: AtomicUsize::new(0),
            result,
        }))
    }

    fn temp_store() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("temporary credential directory");
        let path = temp.path().join("credentials.json");
        (temp, path)
    }

    async fn wait_terminal(
        manager: &BilibiliLoginManager,
        session_id: &str,
    ) -> BilibiliLoginSession {
        for _ in 0..100 {
            let session = manager.get(session_id).expect("session remains available");
            if session.state() != BilibiliLoginSessionState::Pending {
                return session;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("login session did not reach a terminal state");
    }

    #[tokio::test]
    async fn persists_cookie_and_refresh_token_without_dropping_other_profiles_or_secrets() {
        let (_temp, path) = temp_store();
        let store = CredentialStore::new(path.clone());
        store
            .update_profiles(|profiles| {
                profiles.set_profile(
                    "default",
                    Credentials::default()
                        .with_access_key("preserve-access-key")
                        .with_tv_access_key("preserve-tv-key"),
                )?;
                profiles
                    .set_profile("other", Credentials::default().with_cookie("other-cookie"))?;
                let mut secrets = CredentialProfileSecrets::default();
                secrets.set_tv_access_key(
                    CredentialRefreshSecret::default().with_refresh_token("tv-refresh"),
                );
                profiles.set_profile_secrets("default", secrets)
            })
            .expect("seed profiles");

        let manager = manager(PollResult::Succeeded {
            credentials: Credentials::default().with_cookie("web-cookie"),
            refresh_token: Some("web-refresh".to_owned()),
        });
        let session = manager
            .start("default".to_owned(), Some(path.clone()))
            .await
            .expect("start");
        let terminal = wait_terminal(&manager, &session.id).await;

        assert_eq!(BilibiliLoginSessionState::Ready, terminal.state());
        assert!(terminal.verification_uri.is_empty());
        let profiles = store.load_profiles().expect("load persisted profiles");
        let default = profiles.profile("default").expect("default profile");
        assert_eq!(Some("web-cookie"), default.cookie.as_deref());
        assert_eq!(Some("preserve-access-key"), default.access_key.as_deref());
        assert_eq!(Some("preserve-tv-key"), default.tv_access_key.as_deref());
        assert_eq!(
            Some("other-cookie"),
            profiles
                .profile("other")
                .expect("other profile")
                .cookie
                .as_deref()
        );
        let secrets = profiles
            .profile_secrets("default")
            .expect("default secrets");
        assert_eq!(
            Some("web-refresh"),
            secrets
                .cookie()
                .and_then(|secret| secret.refresh_token.as_deref())
        );
        assert_eq!(
            Some("tv-refresh"),
            secrets
                .tv_access_key()
                .and_then(|secret| secret.refresh_token.as_deref())
        );
    }

    #[tokio::test]
    async fn refuses_to_replace_existing_web_cookie() {
        let (_temp, path) = temp_store();
        CredentialStore::new(path.clone())
            .save(&Credentials::default().with_cookie("existing-cookie"))
            .expect("seed cookie");
        let manager = manager(PollResult::Waiting);
        let error = manager
            .start("default".to_owned(), Some(path.clone()))
            .await
            .expect_err("existing cookie is protected");
        assert_eq!(Code::FailedPrecondition, error.code());
        assert_eq!(
            Some("existing-cookie"),
            CredentialStore::new(path)
                .load()
                .expect("load")
                .cookie
                .as_deref()
        );
    }

    #[tokio::test]
    async fn rejects_missing_path_without_exposing_filesystem_details() {
        let manager = manager(PollResult::Waiting);
        let error = manager
            .start("default".to_owned(), None)
            .await
            .expect_err("path is required");
        assert_eq!(Code::FailedPrecondition, error.code());
        assert!(!error.message().contains("/") && !error.message().contains("\\"));
    }

    #[tokio::test]
    async fn rejects_concurrent_start_for_same_profile() {
        let (_temp, path) = temp_store();
        let manager = manager(PollResult::Waiting);
        let (left, right) = tokio::join!(
            manager.start("default".to_owned(), Some(path.clone())),
            manager.start("default".to_owned(), Some(path)),
        );
        let sessions = [&left, &right]
            .into_iter()
            .filter_map(|result| result.as_ref().ok())
            .collect::<Vec<_>>();
        let errors = [&left, &right]
            .into_iter()
            .filter_map(|result| result.as_ref().err())
            .collect::<Vec<_>>();
        assert_eq!(1, sessions.len());
        assert_eq!(1, errors.len());
        assert_eq!(Code::FailedPrecondition, errors[0].code());
        assert_eq!(
            BilibiliLoginSessionState::Pending,
            manager
                .get(&sessions[0].id)
                .expect("winning session")
                .state()
        );
    }

    #[tokio::test]
    async fn cancelling_ticket_creation_releases_profile_reservation() {
        let (_temp, path) = temp_store();
        let provider = Arc::new(PausedCreateProvider::default());
        let manager = BilibiliLoginManager::new(provider.clone());
        let starting_manager = manager.clone();
        let start = tokio::spawn(async move {
            starting_manager
                .start("default".to_owned(), Some(path))
                .await
        });

        provider.started.notified().await;
        start.abort();
        assert!(start.await.is_err());
        provider.resume.notify_one();

        let (_temp, retry_path) = temp_store();
        let retry = manager
            .start("default".to_owned(), Some(retry_path))
            .await
            .expect("cancelled reservation should be released");
        assert_eq!(BilibiliLoginSessionState::Pending, retry.state());
    }

    #[tokio::test]
    async fn expired_session_cannot_commit_after_new_session_takes_profile() {
        let (_temp, path) = temp_store();
        let manager = manager(PollResult::Waiting);
        let expired = manager
            .start("default".to_owned(), Some(path.clone()))
            .await
            .expect("first session");
        {
            let mut state = manager.state.lock().expect("state lock");
            state
                .sessions
                .get_mut(&expired.id)
                .expect("session record")
                .deadline = Instant::now() - Duration::from_secs(1);
            expire_sessions(&mut state, Instant::now());
        }
        let replacement = manager
            .start("default".to_owned(), Some(path.clone()))
            .await
            .expect("replacement session");

        let committed = manager
            .persist_credentials(
                &expired.id,
                path.clone(),
                "default".to_owned(),
                Credentials::default().with_cookie("late-cookie"),
                Some("late-refresh".to_owned()),
            )
            .await;

        assert!(!committed);
        assert_eq!(
            BilibiliLoginSessionState::Expired,
            manager
                .get(&expired.id)
                .expect("expired session remains queryable")
                .state()
        );
        assert!(
            manager
                .get(&expired.id)
                .expect("expired session")
                .verification_uri
                .is_empty()
        );
        assert_eq!(
            BilibiliLoginSessionState::Pending,
            manager
                .get(&replacement.id)
                .expect("replacement remains active")
                .state()
        );
        assert!(
            CredentialStore::new(path)
                .load()
                .expect("load profile")
                .cookie
                .is_none()
        );
    }

    #[test]
    fn claimed_store_write_can_finish_after_qr_deadline() {
        let manager = manager(PollResult::Waiting);
        let session_id = "claimed";
        let profile_id = "default";
        {
            let mut state = manager.state.lock().expect("state lock");
            state
                .active_profiles
                .insert(profile_id.to_owned(), session_id.to_owned());
            state.sessions.insert(
                session_id.to_owned(),
                SessionRecord {
                    public: BilibiliLoginSession {
                        id: session_id.to_owned(),
                        profile_id: profile_id.to_owned(),
                        method: 1,
                        state: BilibiliLoginSessionState::Pending.into(),
                        message: String::new(),
                        verification_uri: String::new(),
                        created_at: None,
                        expires_at: None,
                    },
                    ticket_key: "private-ticket".to_owned(),
                    deadline: Instant::now() - Duration::from_secs(1),
                    finishing: true,
                    completed_at: None,
                },
            );
        }

        assert!(manager.complete_claimed_session(session_id, profile_id, true));
        assert_eq!(
            BilibiliLoginSessionState::Ready,
            manager.get(session_id).expect("completed session").state()
        );
        assert!(
            !manager
                .state
                .lock()
                .expect("state lock")
                .active_profiles
                .contains_key(profile_id)
        );
    }

    #[test]
    fn get_prunes_terminal_sessions_past_completion_ttl() {
        let manager = manager(PollResult::Waiting);
        let session = BilibiliLoginSession {
            id: "completed".to_owned(),
            profile_id: "default".to_owned(),
            method: 1,
            state: BilibiliLoginSessionState::Ready.into(),
            message: "done".to_owned(),
            verification_uri: String::new(),
            created_at: None,
            expires_at: None,
        };
        manager.state.lock().expect("state lock").sessions.insert(
            session.id.clone(),
            SessionRecord {
                public: session,
                ticket_key: String::new(),
                deadline: Instant::now(),
                finishing: false,
                completed_at: Some(Instant::now() - TERMINAL_SESSION_TTL - Duration::from_secs(1)),
            },
        );

        assert_eq!(
            Code::NotFound,
            manager
                .get("completed")
                .expect_err("expired terminal record")
                .code()
        );
        assert!(
            !manager
                .state
                .lock()
                .expect("state lock")
                .sessions
                .contains_key("completed")
        );
    }

    #[tokio::test]
    async fn completed_session_remains_queryable_when_another_profile_starts() {
        let (_temp, path) = temp_store();
        let manager = manager(PollResult::Waiting);
        {
            let mut state = manager.state.lock().expect("state lock");
            state.sessions.insert(
                "completed".to_owned(),
                SessionRecord {
                    public: BilibiliLoginSession {
                        id: "completed".to_owned(),
                        profile_id: "first".to_owned(),
                        method: 1,
                        state: BilibiliLoginSessionState::Ready.into(),
                        message: "done".to_owned(),
                        verification_uri: String::new(),
                        created_at: Some(current_timestamp()),
                        expires_at: Some(timestamp_after(SESSION_TTL)),
                    },
                    ticket_key: "private-ticket".to_owned(),
                    deadline: Instant::now(),
                    finishing: false,
                    completed_at: Some(Instant::now()),
                },
            );
        }

        let _active = manager
            .start("second".to_owned(), Some(path))
            .await
            .expect("other profile starts");
        let retained = manager
            .get("completed")
            .expect("recent terminal session remains queryable");
        assert_eq!(BilibiliLoginSessionState::Ready, retained.state());
        assert_eq!("done", retained.message);
    }

    #[tokio::test]
    async fn capacity_rejects_new_session_without_evicting_active_sessions() {
        let (_temp, path) = temp_store();
        let manager = manager(PollResult::Waiting);
        {
            let mut state = manager.state.lock().expect("state lock");
            for index in 0..MAX_LOGIN_SESSIONS {
                let id = format!("session-{index}");
                let profile = format!("profile-{index}");
                state.active_profiles.insert(profile.clone(), id.clone());
                state.sessions.insert(
                    id.clone(),
                    SessionRecord {
                        public: BilibiliLoginSession {
                            id,
                            profile_id: profile,
                            method: 1,
                            state: BilibiliLoginSessionState::Pending.into(),
                            message: String::new(),
                            verification_uri: String::new(),
                            created_at: None,
                            expires_at: None,
                        },
                        ticket_key: "private-ticket".to_owned(),
                        deadline: Instant::now() + SESSION_TTL,
                        finishing: false,
                        completed_at: None,
                    },
                );
            }
        }

        let error = manager
            .start("overflow".to_owned(), Some(path))
            .await
            .expect_err("full active set rejects login");
        assert_eq!(Code::ResourceExhausted, error.code());
        let state = manager.state.lock().expect("state lock");
        assert_eq!(MAX_LOGIN_SESSIONS, state.sessions.len());
        assert_eq!(MAX_LOGIN_SESSIONS, state.active_profiles.len());
    }

    #[test]
    fn expiration_is_terminal_and_releases_profile_reservation() {
        let mut state = LoginState::default();
        state
            .active_profiles
            .insert("default".to_owned(), "id".to_owned());
        state.sessions.insert(
            "id".to_owned(),
            SessionRecord {
                public: BilibiliLoginSession {
                    id: "id".to_owned(),
                    profile_id: "default".to_owned(),
                    method: 1,
                    state: BilibiliLoginSessionState::Pending.into(),
                    message: String::new(),
                    verification_uri: String::new(),
                    created_at: None,
                    expires_at: None,
                },
                ticket_key: "secret-ticket".to_owned(),
                deadline: Instant::now(),
                finishing: false,
                completed_at: None,
            },
        );
        expire_sessions(&mut state, Instant::now() + Duration::from_secs(1));
        assert_eq!(
            BilibiliLoginSessionState::Expired,
            state.sessions["id"].public.state()
        );
        assert!(!state.active_profiles.contains_key("default"));
    }

    #[tokio::test]
    async fn terminal_completion_is_single_winner_and_error_text_is_redacted() {
        let manager = manager(PollResult::Waiting);
        let profile = "default";
        let session = BilibiliLoginSession {
            id: "id".to_owned(),
            profile_id: profile.to_owned(),
            method: 1,
            state: BilibiliLoginSessionState::Pending.into(),
            message: String::new(),
            verification_uri: String::new(),
            created_at: None,
            expires_at: None,
        };
        {
            let mut state = manager.state.lock().expect("state lock");
            state
                .active_profiles
                .insert(profile.to_owned(), "id".to_owned());
            state.sessions.insert(
                "id".to_owned(),
                SessionRecord {
                    public: session,
                    ticket_key: "secret-ticket".to_owned(),
                    deadline: Instant::now() + SESSION_TTL,
                    finishing: false,
                    completed_at: None,
                },
            );
        }
        let first = manager.clone();
        let second = manager.clone();
        let (first_result, second_result) = tokio::join!(
            tokio::spawn(async move {
                first.finish_without_store(
                    "id",
                    profile,
                    BilibiliLoginSessionState::Error,
                    "safe generic failure",
                );
            }),
            tokio::spawn(async move {
                second.finish_without_store(
                    "id",
                    profile,
                    BilibiliLoginSessionState::Expired,
                    "other terminal result",
                );
            }),
        );
        assert!(first_result.is_ok() && second_result.is_ok());
        let session = manager.get("id").expect("session");
        assert!(matches!(
            session.state(),
            BilibiliLoginSessionState::Error | BilibiliLoginSessionState::Expired
        ));
        assert!(matches!(
            session.message.as_str(),
            "safe generic failure" | "other terminal result"
        ));
        assert!(!session.message.contains("secret-ticket"));
    }

    #[test]
    fn login_capability_requires_usable_configured_store() {
        let (temp, path) = temp_store();
        assert!(credential_login_available(Some(&path), None));
        assert_eq!(
            0,
            fs::read_dir(temp.path())
                .expect("credential parent should remain readable")
                .count()
        );
        assert!(!credential_login_available(None, None));
        assert!(!credential_login_available(
            Some(Path::new("/missing-parent/credentials.json")),
            None
        ));
    }

    #[cfg(unix)]
    #[test]
    fn login_capability_rejects_read_only_credential_parent() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, path) = temp_store();
        let parent = temp.path();
        let original = fs::metadata(parent)
            .expect("credential parent metadata")
            .permissions();
        let mut read_only = original.clone();
        read_only.set_mode(original.mode() & !0o222);
        fs::set_permissions(parent, read_only).expect("make credential parent read-only");

        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping permission assertion when running as root");
        } else {
            assert!(!credential_login_available(Some(&path), None));
        }

        fs::set_permissions(parent, original).expect("restore credential parent permissions");
    }

    #[test]
    fn credential_store_drops_cookie_refresh_secret_without_cookie() {
        let (_temp, path) = temp_store();
        fs::write(
            &path,
            r#"{
  "version": 1,
  "default_profile": "default",
  "profiles": {
    "default": { "access_key": "access-key-fixture" }
  },
  "profile_secrets": {
    "default": { "cookie": { "refresh_token": "stale-refresh-fixture" } }
  }
}"#,
        )
        .expect("write raw credential profile");

        let profiles = CredentialStore::new(path)
            .load_profiles()
            .expect("load normalized profiles");
        assert_eq!(
            Some("access-key-fixture"),
            profiles
                .profile("default")
                .expect("default profile")
                .access_key
                .as_deref()
        );
        assert!(
            profiles
                .profile_secrets("default")
                .expect("profile secrets")
                .cookie()
                .is_none()
        );
    }
}
