use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, atomic::AtomicU8},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bbdown_core::{
    AccessKeyLoginConfig, AccessKeyLoginCredentials, AccessKeyLoginTicket, AccessKeyProvider,
    AccessKeyProviderSecret, BiliClient, ClientConfig, CredentialKind, CredentialProfileSecrets,
    CredentialRefreshSecret, CredentialStore, Credentials, QrLoginCredentialsState,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::time::{sleep, timeout};
use tonic::Status;

use crate::{
    bilibili_credential_bindings::{self, BindingError, BindingLookup, VerifiedWebBinding},
    bilibili_credentials::{
        BbdownCredentialProvider, CREDENTIAL_PROVIDER_TIMEOUT, CredentialProvider,
        CredentialProviderError, CredentialReadiness, CredentialReadinessSnapshot,
    },
    generated::tvos_net_player::v1::{BilibiliLoginSession, BilibiliLoginSessionState},
    task_registry::current_timestamp,
};

const MAX_LOGIN_SESSIONS: usize = 64;
const SESSION_TTL: Duration = Duration::from_secs(180);
const TERMINAL_SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_QR_URL_BYTES: usize = 4096;
pub(crate) const MAX_BROWSER_MESSAGE_BYTES: usize = 16 * 1024;
const BROWSER_COMPLETION_ACTIVE: u8 = 0;
const BROWSER_COMPLETION_CANCELLED: u8 = 1;
const BROWSER_COMPLETION_COMMITTING: u8 = 2;
const BROWSER_COMPLETION_FINISHED: u8 = 3;

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
    credential_path: Option<PathBuf>,
    method: LoginMethod,
    ticket_key: Option<String>,
    access_ticket: Option<AccessKeyLoginTicket>,
    capability: Option<String>,
    server_origin: Option<String>,
    expected_account_id: Option<u64>,
    expected_web_binding: Option<VerifiedWebBinding>,
    baseline: CredentialBaseline,
    deadline: Instant,
    finishing: bool,
    completed_at: Option<Instant>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LoginMethod {
    WebQr,
    AccessKeyBrowser,
}

#[derive(Clone, Default)]
struct CredentialBaseline {
    web_cookie: Option<String>,
    web_refresh: Option<String>,
    access_key: Option<String>,
    access_secrets: Option<String>,
}

struct ReadinessRecord {
    snapshot: CredentialReadinessSnapshot,
    web_cookie: Option<String>,
    access_key: Option<String>,
    all_credentials: bool,
    operation_id: Option<String>,
    bundle_fingerprint: Option<String>,
}

struct ProfileCredentials {
    credentials: Credentials,
    secrets: CredentialProfileSecrets,
}

enum WebMaintenance {
    Missing {
        binding: Option<VerifiedWebBinding>,
        credentials: Credentials,
        secrets: CredentialProfileSecrets,
    },
    Ready {
        binding: VerifiedWebBinding,
        credentials: Credentials,
        secrets: CredentialProfileSecrets,
    },
    Reauthorization {
        binding: VerifiedWebBinding,
        credentials: Credentials,
        secrets: CredentialProfileSecrets,
    },
    Failed(CredentialReadiness),
}

enum CredentialCommitError {
    Changed,
    Unavailable,
}

enum BrowserCommitClaim {
    Claimed,
    Cancelled,
    Expired,
    Inactive,
    Unavailable,
}

enum BrowserCommitOutcome {
    Completed {
        result: Result<(), CredentialCommitError>,
        session_completed: bool,
    },
    Cancelled,
    Expired,
    Inactive,
    Unavailable,
}

struct BrowserAccessKeyCommit {
    session_id: String,
    profile_id: String,
    path: PathBuf,
    baseline: CredentialBaseline,
    expected_binding: Option<VerifiedWebBinding>,
    credentials: AccessKeyLoginCredentials,
    phase: Arc<AtomicU8>,
}

#[derive(Default)]
struct LoginState {
    sessions: HashMap<String, SessionRecord>,
    active_profiles: HashMap<String, String>,
    readiness: HashMap<String, ReadinessRecord>,
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
                .cancel_profile_operation(&self.profile_id, &self.session_id);
        }
    }
}

struct BrowserCompletionGuard {
    manager: BilibiliLoginManager,
    profile_id: String,
    session_id: String,
    phase: Arc<AtomicU8>,
}

impl BrowserCompletionGuard {
    fn new(manager: &BilibiliLoginManager, profile_id: &str, session_id: &str) -> Self {
        Self {
            manager: manager.clone(),
            profile_id: profile_id.to_owned(),
            session_id: session_id.to_owned(),
            phase: Arc::new(AtomicU8::new(BROWSER_COMPLETION_ACTIVE)),
        }
    }

    fn phase(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.phase)
    }

    fn finish(&self) {
        self.phase.store(
            BROWSER_COMPLETION_FINISHED,
            std::sync::atomic::Ordering::Release,
        );
    }
}

impl Drop for BrowserCompletionGuard {
    fn drop(&mut self) {
        if self
            .phase
            .compare_exchange(
                BROWSER_COMPLETION_ACTIVE,
                BROWSER_COMPLETION_CANCELLED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            self.manager
                .cancel_browser_completion(&self.session_id, &self.profile_id);
        }
    }
}

struct BrowserCommitWorkerGuard {
    manager: BilibiliLoginManager,
    profile_id: String,
    session_id: String,
    phase: Arc<AtomicU8>,
    finished: bool,
}

impl BrowserCommitWorkerGuard {
    fn new(
        manager: BilibiliLoginManager,
        profile_id: String,
        session_id: String,
        phase: Arc<AtomicU8>,
    ) -> Self {
        Self {
            manager,
            profile_id,
            session_id,
            phase,
            finished: false,
        }
    }

    fn finish(&mut self, saved: bool) -> bool {
        let completed =
            self.manager
                .complete_claimed_session(&self.session_id, &self.profile_id, saved);
        self.finished = true;
        self.phase.store(
            BROWSER_COMPLETION_FINISHED,
            std::sync::atomic::Ordering::Release,
        );
        completed
    }
}

impl Drop for BrowserCommitWorkerGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.manager
                .complete_claimed_session(&self.session_id, &self.profile_id, false);
            self.phase.store(
                BROWSER_COMPLETION_FINISHED,
                std::sync::atomic::Ordering::Release,
            );
        }
    }
}

#[derive(Clone)]
pub(crate) struct BilibiliLoginManager {
    state: Arc<Mutex<LoginState>>,
    provider: Arc<dyn WebQrProvider>,
    credential_provider: Arc<dyn CredentialProvider>,
    credential_timeout: Duration,
}

impl Default for BilibiliLoginManager {
    fn default() -> Self {
        Self::with_providers(
            Arc::new(BbdownWebQrProvider),
            Arc::new(BbdownCredentialProvider),
        )
    }
}

impl BilibiliLoginManager {
    #[cfg(test)]
    fn new(provider: Arc<dyn WebQrProvider>) -> Self {
        Self::with_providers(provider, Arc::new(BbdownCredentialProvider))
    }

    fn with_providers(
        provider: Arc<dyn WebQrProvider>,
        credential_provider: Arc<dyn CredentialProvider>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(LoginState::default())),
            provider,
            credential_provider,
            credential_timeout: CREDENTIAL_PROVIDER_TIMEOUT,
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
        let mut reservation = self.reserve_profile(&profile_id, &session_id)?;
        let loaded = load_profile_bundle(&path, &profile_id)?;
        self.set_readiness(
            &profile_id,
            &loaded.credentials,
            CredentialReadinessSnapshot {
                web: if has_cookie(&loaded.credentials) {
                    CredentialReadiness::Checking
                } else {
                    CredentialReadiness::Missing
                },
                access_key: if has_access_key(&loaded.credentials) {
                    CredentialReadiness::Checking
                } else {
                    CredentialReadiness::Missing
                },
            },
        );

        match self
            .maintain_web(&path, &profile_id, loaded.credentials, loaded.secrets)
            .await
        {
            WebMaintenance::Ready {
                binding,
                credentials,
                secrets,
            } => {
                self.set_readiness(
                    &profile_id,
                    &credentials,
                    CredentialReadinessSnapshot {
                        web: CredentialReadiness::Ready,
                        access_key: if has_access_key(&credentials) {
                            CredentialReadiness::Unknown
                        } else {
                            CredentialReadiness::Missing
                        },
                    },
                );
                let session = ready_session(
                    &session_id,
                    &profile_id,
                    LoginMethod::WebQr,
                    "Bilibili Web credentials are already ready.",
                );
                self.insert_terminal_session(session.clone(), LoginMethod::WebQr)?;
                let _ = (binding, secrets);
                reservation.disarm();
                self.release_profile(&profile_id, &session_id);
                Ok(session)
            }
            WebMaintenance::Missing {
                binding,
                credentials,
                secrets,
            } => {
                self.start_web_qr(&mut reservation, path, credentials, secrets, binding)
                    .await
            }
            WebMaintenance::Reauthorization {
                binding,
                credentials,
                secrets,
            } => {
                self.start_web_qr(&mut reservation, path, credentials, secrets, Some(binding))
                    .await
            }
            WebMaintenance::Failed(readiness) => {
                let current = load_profile_bundle(&path, &profile_id).ok();
                if let Some(current) = current {
                    self.set_readiness(
                        &profile_id,
                        &current.credentials,
                        CredentialReadinessSnapshot {
                            web: readiness,
                            access_key: if has_access_key(&current.credentials) {
                                CredentialReadiness::Unknown
                            } else {
                                CredentialReadiness::Missing
                            },
                        },
                    );
                }
                Err(readiness_status(readiness))
            }
        }
    }

    async fn start_web_qr(
        &self,
        reservation: &mut ProfileReservationGuard,
        path: PathBuf,
        credentials: Credentials,
        secrets: CredentialProfileSecrets,
        expected_binding: Option<VerifiedWebBinding>,
    ) -> Result<BilibiliLoginSession, Status> {
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
            id: reservation.session_id.clone(),
            profile_id: reservation.profile_id.clone(),
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
                    credential_path: Some(path.clone()),
                    method: LoginMethod::WebQr,
                    ticket_key: Some(ticket.key),
                    access_ticket: None,
                    capability: None,
                    server_origin: None,
                    expected_account_id: expected_binding
                        .as_ref()
                        .map(VerifiedWebBinding::account_id),
                    expected_web_binding: expected_binding,
                    baseline: credential_baseline(&credentials, &secrets),
                    deadline: now + SESSION_TTL,
                    finishing: false,
                    completed_at: None,
                },
            );
        }
        self.set_readiness(
            &reservation.profile_id,
            &credentials,
            CredentialReadinessSnapshot {
                web: CredentialReadiness::Checking,
                access_key: if has_access_key(&credentials) {
                    CredentialReadiness::Unknown
                } else {
                    CredentialReadiness::Missing
                },
            },
        );
        let manager = self.clone();
        let poll_id = session.id.clone();
        tokio::spawn(async move { manager.run_session(poll_id, path).await });
        reservation.disarm();
        Ok(session)
    }

    fn insert_terminal_session(
        &self,
        session: BilibiliLoginSession,
        method: LoginMethod,
    ) -> Result<(), Status> {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?;
        expire_sessions(&mut state, now);
        prune_terminal_sessions(&mut state, now);
        while state.sessions.len() >= MAX_LOGIN_SESSIONS
            && evict_oldest_terminal_session(&mut state)
        {}
        if state.sessions.len() >= MAX_LOGIN_SESSIONS {
            return Err(Status::resource_exhausted(
                "Too many Bilibili login sessions are active.",
            ));
        }
        state.sessions.insert(
            session.id.clone(),
            SessionRecord {
                public: session,
                credential_path: None,
                method,
                ticket_key: None,
                access_ticket: None,
                capability: None,
                server_origin: None,
                expected_account_id: None,
                expected_web_binding: None,
                baseline: CredentialBaseline::default(),
                deadline: now,
                finishing: false,
                completed_at: Some(now),
            },
        );
        Ok(())
    }

    fn expire_session(&self, session_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            expire_sessions(&mut state, Instant::now());
            if let Some(record) = state.sessions.get_mut(session_id)
                && record.public.state() == BilibiliLoginSessionState::Pending
            {
                record.public.state = BilibiliLoginSessionState::Expired.into();
                record.public.message = "The login session expired.".to_owned();
                clear_private_session(record);
                record.completed_at = Some(Instant::now());
                let profile_id = record.public.profile_id.clone();
                self.release_profile_locked(&mut state, &profile_id, session_id);
            }
        }
    }

    pub(crate) async fn maintain_profile(&self, profile_id: String, path: PathBuf) {
        let operation_id = uuid::Uuid::new_v4().to_string();
        let mut reservation = match self.reserve_profile(&profile_id, &operation_id) {
            Ok(reservation) => reservation,
            Err(_) => return,
        };
        let loaded = match load_profile_bundle(&path, &profile_id) {
            Ok(loaded) => loaded,
            Err(_) => {
                self.set_unavailable_readiness(&profile_id);
                return;
            }
        };
        self.set_readiness(
            &profile_id,
            &loaded.credentials,
            CredentialReadinessSnapshot {
                web: if has_cookie(&loaded.credentials) {
                    CredentialReadiness::Checking
                } else {
                    CredentialReadiness::Missing
                },
                access_key: if has_access_key(&loaded.credentials) {
                    CredentialReadiness::Checking
                } else {
                    CredentialReadiness::Missing
                },
            },
        );

        let (web, binding, credentials) = match self
            .maintain_web(&path, &profile_id, loaded.credentials, loaded.secrets)
            .await
        {
            WebMaintenance::Missing {
                binding,
                credentials,
                ..
            } => (CredentialReadiness::Missing, binding, credentials),
            WebMaintenance::Ready {
                binding,
                credentials,
                ..
            } => (CredentialReadiness::Ready, Some(binding), credentials),
            WebMaintenance::Reauthorization {
                binding,
                credentials,
                ..
            } => (
                CredentialReadiness::LoginRequired,
                Some(binding),
                credentials,
            ),
            WebMaintenance::Failed(readiness) => {
                let current = load_profile_bundle(&path, &profile_id)
                    .map(|loaded| loaded.credentials)
                    .unwrap_or_default();
                let access_key = self
                    .maintain_access_key(
                        &current,
                        binding_from_path(&path, &profile_id).ok().flatten(),
                    )
                    .await;
                self.set_readiness(
                    &profile_id,
                    &current,
                    CredentialReadinessSnapshot {
                        web: readiness,
                        access_key,
                    },
                );
                reservation.disarm();
                self.release_profile(&profile_id, &operation_id);
                return;
            }
        };
        let access_key = self.maintain_access_key(&credentials, binding).await;
        self.set_readiness(
            &profile_id,
            &credentials,
            CredentialReadinessSnapshot { web, access_key },
        );
        reservation.disarm();
        self.release_profile(&profile_id, &operation_id);
    }

    pub(crate) fn readiness(
        &self,
        profile_id: &str,
        credentials: &Credentials,
    ) -> CredentialReadinessSnapshot {
        let fallback = CredentialReadinessSnapshot {
            web: if has_cookie(credentials) {
                CredentialReadiness::Unknown
            } else {
                CredentialReadiness::Missing
            },
            access_key: if has_access_key(credentials) {
                CredentialReadiness::Unknown
            } else {
                CredentialReadiness::Missing
            },
        };
        let Ok(state) = self.state.lock() else {
            return CredentialReadinessSnapshot {
                web: if has_cookie(credentials) {
                    CredentialReadiness::Unavailable
                } else {
                    CredentialReadiness::Missing
                },
                access_key: if has_access_key(credentials) {
                    CredentialReadiness::Unavailable
                } else {
                    CredentialReadiness::Missing
                },
            };
        };
        let Some(record) = state.readiness.get(profile_id) else {
            return if state.active_profiles.contains_key(profile_id) {
                CredentialReadinessSnapshot {
                    web: if has_cookie(credentials) {
                        CredentialReadiness::Checking
                    } else {
                        CredentialReadiness::Missing
                    },
                    access_key: if has_access_key(credentials) {
                        CredentialReadiness::Checking
                    } else {
                        CredentialReadiness::Missing
                    },
                }
            } else {
                fallback
            };
        };
        if record.all_credentials
            || (record.web_cookie == credential_fingerprint(credentials.cookie.as_deref())
                && record.access_key == credential_fingerprint(credentials.access_key.as_deref()))
        {
            record.snapshot
        } else {
            fallback
        }
    }

    async fn maintain_web(
        &self,
        path: &Path,
        profile_id: &str,
        mut credentials: Credentials,
        mut secrets: CredentialProfileSecrets,
    ) -> WebMaintenance {
        let Some(cookie) = credentials
            .cookie
            .as_deref()
            .filter(|cookie| !cookie.trim().is_empty())
            .map(str::to_owned)
        else {
            return match binding_from_path(path, profile_id) {
                Ok(binding) => WebMaintenance::Missing {
                    binding,
                    credentials,
                    secrets,
                },
                Err(_) => WebMaintenance::Failed(CredentialReadiness::Unavailable),
            };
        };
        let stored_binding = match bilibili_credential_bindings::lookup(path, profile_id, &cookie) {
            Ok(BindingLookup::Matches(binding)) => Some((binding.clone(), Some(binding))),
            Ok(BindingLookup::FingerprintMismatch(binding)) => Some((binding, None)),
            Ok(BindingLookup::Missing) => None,
            Err(_) => return WebMaintenance::Failed(CredentialReadiness::Unavailable),
        };
        let identity = match timeout(
            self.credential_timeout,
            self.credential_provider
                .account_identity(credentials.clone(), CredentialKind::Cookie),
        )
        .await
        {
            Ok(Ok(identity)) if identity.kind == CredentialKind::Cookie => identity,
            Ok(Ok(_)) => return WebMaintenance::Failed(CredentialReadiness::Unavailable),
            Ok(Err(CredentialProviderError::Rejected)) => {
                let Some((binding, _)) = stored_binding.as_ref() else {
                    return WebMaintenance::Failed(CredentialReadiness::LoginRequired);
                };
                return self
                    .refresh_rejected_web(
                        path,
                        profile_id,
                        &cookie,
                        binding.clone(),
                        credentials,
                        secrets,
                    )
                    .await;
            }
            Ok(Err(CredentialProviderError::Unavailable)) | Err(_) => {
                return WebMaintenance::Failed(CredentialReadiness::Unavailable);
            }
        };

        let (expected_binding, exact_binding) = match stored_binding {
            Some((binding, exact)) if binding.account_id() == identity.account_id => {
                (Some(binding), exact.is_some())
            }
            Some(_) => return WebMaintenance::Failed(CredentialReadiness::LoginRequired),
            None => (None, false),
        };
        let binding = if exact_binding {
            expected_binding.expect("exact binding is present")
        } else {
            match publish_verified_binding(
                path,
                profile_id,
                &credentials,
                &secrets,
                expected_binding.as_ref(),
                identity.account_id,
            ) {
                Ok(binding) => binding,
                Err(_) => return WebMaintenance::Failed(CredentialReadiness::Unavailable),
            }
        };

        let refresh_token = secrets
            .cookie()
            .and_then(|secret| secret.refresh_token.as_deref())
            .filter(|token| !token.trim().is_empty())
            .map(str::to_owned);
        let Some(refresh_token) = refresh_token else {
            return WebMaintenance::Ready {
                binding,
                credentials,
                secrets,
            };
        };
        let refreshed = match timeout(
            self.credential_timeout,
            self.credential_provider
                .refresh_web_cookie(cookie.clone(), refresh_token.clone()),
        )
        .await
        {
            Ok(Ok(refreshed)) => refreshed,
            Ok(Err(CredentialProviderError::Rejected)) => {
                return WebMaintenance::Reauthorization {
                    binding,
                    credentials,
                    secrets,
                };
            }
            Ok(Err(CredentialProviderError::Unavailable)) | Err(_) => {
                return WebMaintenance::Failed(CredentialReadiness::Unavailable);
            }
        };
        if !refreshed.refreshed {
            return WebMaintenance::Ready {
                binding,
                credentials,
                secrets,
            };
        }
        if refreshed.cookie.trim().is_empty() {
            return WebMaintenance::Failed(CredentialReadiness::Unavailable);
        }
        let refreshed_identity = match timeout(
            self.credential_timeout,
            self.credential_provider.account_identity(
                Credentials::default().with_cookie(refreshed.cookie.clone()),
                CredentialKind::Cookie,
            ),
        )
        .await
        {
            Ok(Ok(identity)) if identity.kind == CredentialKind::Cookie => identity,
            Ok(Err(CredentialProviderError::Rejected)) => {
                return WebMaintenance::Reauthorization {
                    binding,
                    credentials,
                    secrets,
                };
            }
            _ => return WebMaintenance::Failed(CredentialReadiness::Unavailable),
        };
        if refreshed_identity.account_id != binding.account_id() {
            return WebMaintenance::Failed(CredentialReadiness::LoginRequired);
        }

        let new_refresh = if refreshed.refresh_token.trim().is_empty() {
            Some(refresh_token.clone())
        } else {
            Some(refreshed.refresh_token.clone())
        };
        let baseline = credential_baseline(&credentials, &secrets);
        let new_binding = match persist_refreshed_web(
            path,
            profile_id,
            &baseline,
            &binding,
            &refreshed.cookie,
            new_refresh.as_deref(),
            refreshed_identity.account_id,
        ) {
            Ok(binding) => binding,
            Err(_) => return WebMaintenance::Failed(CredentialReadiness::Unavailable),
        };
        credentials.cookie = Some(refreshed.cookie);
        secrets.set_cookie(
            new_refresh.map_or_else(CredentialRefreshSecret::default, |value| {
                CredentialRefreshSecret::default().with_refresh_token(value)
            }),
        );
        WebMaintenance::Ready {
            binding: new_binding,
            credentials,
            secrets,
        }
    }

    async fn refresh_rejected_web(
        &self,
        path: &Path,
        profile_id: &str,
        cookie: &str,
        binding: VerifiedWebBinding,
        mut credentials: Credentials,
        mut secrets: CredentialProfileSecrets,
    ) -> WebMaintenance {
        let refresh_token = secrets
            .cookie()
            .and_then(|secret| secret.refresh_token.as_deref())
            .filter(|token| !token.trim().is_empty())
            .map(str::to_owned);
        let Some(refresh_token) = refresh_token else {
            return WebMaintenance::Reauthorization {
                binding,
                credentials,
                secrets,
            };
        };
        let refreshed = match timeout(
            self.credential_timeout,
            self.credential_provider
                .refresh_web_cookie(cookie.to_owned(), refresh_token.clone()),
        )
        .await
        {
            Ok(Ok(refreshed)) if refreshed.refreshed => refreshed,
            Ok(Ok(_)) | Ok(Err(CredentialProviderError::Rejected)) => {
                return WebMaintenance::Reauthorization {
                    binding,
                    credentials,
                    secrets,
                };
            }
            Ok(Err(CredentialProviderError::Unavailable)) | Err(_) => {
                return WebMaintenance::Failed(CredentialReadiness::Unavailable);
            }
        };
        if refreshed.cookie.trim().is_empty() {
            return WebMaintenance::Failed(CredentialReadiness::Unavailable);
        }
        let identity = match timeout(
            self.credential_timeout,
            self.credential_provider.account_identity(
                Credentials::default().with_cookie(refreshed.cookie.clone()),
                CredentialKind::Cookie,
            ),
        )
        .await
        {
            Ok(Ok(identity)) if identity.kind == CredentialKind::Cookie => identity,
            Ok(Err(CredentialProviderError::Rejected)) => {
                return WebMaintenance::Reauthorization {
                    binding,
                    credentials,
                    secrets,
                };
            }
            _ => return WebMaintenance::Failed(CredentialReadiness::Unavailable),
        };
        if identity.account_id != binding.account_id() {
            return WebMaintenance::Failed(CredentialReadiness::LoginRequired);
        }

        let new_refresh = if refreshed.refresh_token.trim().is_empty() {
            refresh_token
        } else {
            refreshed.refresh_token.clone()
        };
        let baseline = credential_baseline(&credentials, &secrets);
        let new_binding = match persist_refreshed_web(
            path,
            profile_id,
            &baseline,
            &binding,
            &refreshed.cookie,
            Some(&new_refresh),
            identity.account_id,
        ) {
            Ok(binding) => binding,
            Err(_) => return WebMaintenance::Failed(CredentialReadiness::Unavailable),
        };
        credentials.cookie = Some(refreshed.cookie);
        secrets.set_cookie(CredentialRefreshSecret::default().with_refresh_token(new_refresh));
        WebMaintenance::Ready {
            binding: new_binding,
            credentials,
            secrets,
        }
    }

    async fn maintain_access_key(
        &self,
        credentials: &Credentials,
        web_binding: Option<VerifiedWebBinding>,
    ) -> CredentialReadiness {
        if !has_access_key(credentials) {
            return CredentialReadiness::Missing;
        }
        match timeout(
            self.credential_timeout,
            self.credential_provider
                .account_identity(credentials.clone(), CredentialKind::AccessKey),
        )
        .await
        {
            Ok(Ok(identity))
                if identity.kind == CredentialKind::AccessKey
                    && web_binding
                        .as_ref()
                        .is_none_or(|binding| binding.account_id() == identity.account_id) =>
            {
                CredentialReadiness::Ready
            }
            Ok(Ok(_)) | Ok(Err(CredentialProviderError::Rejected)) => {
                CredentialReadiness::LoginRequired
            }
            Ok(Err(CredentialProviderError::Unavailable)) | Err(_) => {
                CredentialReadiness::Unavailable
            }
        }
    }

    async fn verify_preserved_access_key_after_web_login(&self, profile_id: &str, path: &Path) {
        let operation_id = uuid::Uuid::new_v4().to_string();
        let mut reservation = match self.reserve_profile(profile_id, &operation_id) {
            Ok(reservation) => reservation,
            Err(_) => return,
        };
        let loaded = match load_profile_bundle(path, profile_id) {
            Ok(loaded) => loaded,
            Err(_) => return,
        };
        if !has_access_key(&loaded.credentials) {
            reservation.disarm();
            self.release_profile(profile_id, &operation_id);
            return;
        }
        let Some(bundle_fingerprint) =
            credential_bundle_fingerprint(&loaded.credentials, &loaded.secrets)
        else {
            reservation.disarm();
            self.release_profile(profile_id, &operation_id);
            return;
        };
        if !self.begin_temporary_access_key_check(
            profile_id,
            &operation_id,
            &loaded.credentials,
            &bundle_fingerprint,
        ) {
            reservation.disarm();
            self.release_profile(profile_id, &operation_id);
            return;
        }

        let cookie = loaded.credentials.cookie.as_deref().unwrap_or_default();
        let binding = match bilibili_credential_bindings::lookup(path, profile_id, cookie) {
            Ok(BindingLookup::Matches(binding)) if has_cookie(&loaded.credentials) => binding,
            _ => {
                self.clear_temporary_readiness(profile_id, &operation_id, &bundle_fingerprint);
                reservation.disarm();
                self.release_profile(profile_id, &operation_id);
                return;
            }
        };
        let access_key_readiness = self
            .maintain_access_key(&loaded.credentials, Some(binding.clone()))
            .await;

        let current = load_profile_bundle(path, profile_id).ok();
        let current_is_same = current.as_ref().is_some_and(|current| {
            credential_bundle_fingerprint(&current.credentials, &current.secrets).as_deref()
                == Some(bundle_fingerprint.as_str())
                && current.credentials.cookie.as_deref().is_some_and(|cookie| {
                    matches!(
                        bilibili_credential_bindings::lookup(path, profile_id, cookie),
                        Ok(BindingLookup::Matches(ref current_binding))
                            if current_binding == &binding
                    )
                })
        });

        let published = current_is_same
            && current.as_ref().is_some_and(|current| {
                self.publish_temporary_access_key_readiness(
                    profile_id,
                    &operation_id,
                    &current.credentials,
                    &bundle_fingerprint,
                    access_key_readiness,
                )
            });
        if !published {
            self.clear_temporary_readiness(profile_id, &operation_id, &bundle_fingerprint);
        }
        reservation.disarm();
        self.release_profile(profile_id, &operation_id);
    }

    fn reserve_profile(
        &self,
        profile_id: &str,
        operation_id: &str,
    ) -> Result<ProfileReservationGuard, Status> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?;
        let now = Instant::now();
        expire_sessions(&mut state, now);
        prune_terminal_sessions(&mut state, now);
        if state.active_profiles.contains_key(profile_id) {
            return Err(Status::failed_precondition(
                "A Bilibili credential operation is already active for this profile.",
            ));
        }
        let mut unmaterialized = state
            .active_profiles
            .values()
            .filter(|active_id| !state.sessions.contains_key(*active_id))
            .count();
        while state.sessions.len() + unmaterialized >= MAX_LOGIN_SESSIONS
            && evict_oldest_terminal_session(&mut state)
        {
            unmaterialized = state
                .active_profiles
                .values()
                .filter(|active_id| !state.sessions.contains_key(*active_id))
                .count();
        }
        if state.sessions.len() + unmaterialized >= MAX_LOGIN_SESSIONS {
            return Err(Status::resource_exhausted(
                "Too many Bilibili credential operations are active.",
            ));
        }
        state
            .active_profiles
            .insert(profile_id.to_owned(), operation_id.to_owned());
        Ok(ProfileReservationGuard::new(
            self.clone(),
            profile_id.to_owned(),
            operation_id.to_owned(),
        ))
    }

    fn set_readiness(
        &self,
        profile_id: &str,
        credentials: &Credentials,
        snapshot: CredentialReadinessSnapshot,
    ) {
        if let Ok(mut state) = self.state.lock() {
            state.readiness.insert(
                profile_id.to_owned(),
                ReadinessRecord {
                    snapshot,
                    web_cookie: credential_fingerprint(credentials.cookie.as_deref()),
                    access_key: credential_fingerprint(credentials.access_key.as_deref()),
                    all_credentials: false,
                    operation_id: None,
                    bundle_fingerprint: None,
                },
            );
        }
    }

    fn begin_temporary_access_key_check(
        &self,
        profile_id: &str,
        operation_id: &str,
        credentials: &Credentials,
        bundle_fingerprint: &str,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.active_profiles.get(profile_id).map(String::as_str) != Some(operation_id) {
            return false;
        }
        let Some(previous) = state.readiness.get(profile_id) else {
            return false;
        };
        if previous.all_credentials
            || previous.web_cookie != credential_fingerprint(credentials.cookie.as_deref())
            || previous.access_key != credential_fingerprint(credentials.access_key.as_deref())
            || previous.operation_id.is_some()
        {
            return false;
        }
        let web = previous.snapshot.web;
        state.readiness.insert(
            profile_id.to_owned(),
            ReadinessRecord {
                snapshot: CredentialReadinessSnapshot {
                    web,
                    access_key: CredentialReadiness::Checking,
                },
                web_cookie: credential_fingerprint(credentials.cookie.as_deref()),
                access_key: credential_fingerprint(credentials.access_key.as_deref()),
                all_credentials: false,
                operation_id: Some(operation_id.to_owned()),
                bundle_fingerprint: Some(bundle_fingerprint.to_owned()),
            },
        );
        true
    }

    fn publish_temporary_access_key_readiness(
        &self,
        profile_id: &str,
        operation_id: &str,
        credentials: &Credentials,
        bundle_fingerprint: &str,
        access_key: CredentialReadiness,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.active_profiles.get(profile_id).map(String::as_str) != Some(operation_id) {
            return false;
        }
        let Some(temporary) = state.readiness.get(profile_id) else {
            return false;
        };
        if temporary.operation_id.as_deref() != Some(operation_id)
            || temporary.bundle_fingerprint.as_deref() != Some(bundle_fingerprint)
            || temporary.web_cookie != credential_fingerprint(credentials.cookie.as_deref())
            || temporary.access_key != credential_fingerprint(credentials.access_key.as_deref())
        {
            return false;
        }
        let web = temporary.snapshot.web;
        state.readiness.insert(
            profile_id.to_owned(),
            ReadinessRecord {
                snapshot: CredentialReadinessSnapshot { web, access_key },
                web_cookie: credential_fingerprint(credentials.cookie.as_deref()),
                access_key: credential_fingerprint(credentials.access_key.as_deref()),
                all_credentials: false,
                operation_id: None,
                bundle_fingerprint: None,
            },
        );
        true
    }

    fn clear_temporary_readiness(
        &self,
        profile_id: &str,
        operation_id: &str,
        bundle_fingerprint: &str,
    ) {
        if let Ok(mut state) = self.state.lock() {
            let owns_reservation =
                state.active_profiles.get(profile_id).map(String::as_str) == Some(operation_id);
            let owns_readiness = state.readiness.get(profile_id).is_some_and(|record| {
                record.operation_id.as_deref() == Some(operation_id)
                    && record.bundle_fingerprint.as_deref() == Some(bundle_fingerprint)
            });
            if owns_reservation && owns_readiness {
                state.readiness.remove(profile_id);
            }
        }
    }

    fn set_unavailable_readiness(&self, profile_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.readiness.insert(
                profile_id.to_owned(),
                ReadinessRecord {
                    snapshot: CredentialReadinessSnapshot {
                        web: CredentialReadiness::Unavailable,
                        access_key: CredentialReadiness::Unavailable,
                    },
                    web_cookie: None,
                    access_key: None,
                    all_credentials: true,
                    operation_id: None,
                    bundle_fingerprint: None,
                },
            );
        }
    }

    fn cancel_profile_operation(&self, profile_id: &str, operation_id: &str) {
        if let Ok(mut state) = self.state.lock()
            && state.active_profiles.get(profile_id).map(String::as_str) == Some(operation_id)
        {
            if state
                .readiness
                .get(profile_id)
                .is_some_and(|record| record.operation_id.as_deref() == Some(operation_id))
            {
                state.readiness.remove(profile_id);
            } else if let Some(record) = state.readiness.get_mut(profile_id) {
                if record.snapshot.web == CredentialReadiness::Checking {
                    record.snapshot.web = CredentialReadiness::Unavailable;
                }
                if record.snapshot.access_key == CredentialReadiness::Checking {
                    record.snapshot.access_key = CredentialReadiness::Unavailable;
                }
            }
            self.release_profile_locked(&mut state, profile_id, operation_id);
        }
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

    pub(crate) async fn start_access_key(
        &self,
        profile_id: String,
        credential_path: Option<PathBuf>,
        login_base_uri: &str,
    ) -> Result<BilibiliLoginSession, Status> {
        let path = credential_path.ok_or_else(|| {
            Status::failed_precondition(
                "Configure BBDown credential storage on the server before starting login.",
            )
        })?;
        let (server_origin, base_path) = parse_login_base_uri(login_base_uri)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut reservation = self.reserve_profile(&profile_id, &session_id)?;
        let loaded = load_profile_bundle(&path, &profile_id)?;
        self.set_readiness(
            &profile_id,
            &loaded.credentials,
            CredentialReadinessSnapshot {
                web: if has_cookie(&loaded.credentials) {
                    CredentialReadiness::Checking
                } else {
                    CredentialReadiness::Missing
                },
                access_key: if has_access_key(&loaded.credentials) {
                    CredentialReadiness::Checking
                } else {
                    CredentialReadiness::Missing
                },
            },
        );
        let (web_readiness, binding, credentials, secrets) = match self
            .maintain_web(&path, &profile_id, loaded.credentials, loaded.secrets)
            .await
        {
            WebMaintenance::Missing {
                binding,
                credentials,
                secrets,
            } => (CredentialReadiness::Missing, binding, credentials, secrets),
            WebMaintenance::Ready {
                binding,
                credentials,
                secrets,
            } => (
                CredentialReadiness::Ready,
                Some(binding),
                credentials,
                secrets,
            ),
            WebMaintenance::Reauthorization {
                binding,
                credentials,
                secrets,
            } => (
                CredentialReadiness::LoginRequired,
                Some(binding),
                credentials,
                secrets,
            ),
            WebMaintenance::Failed(readiness) => {
                return Err(readiness_status(readiness));
            }
        };
        let Some(binding) = binding else {
            self.set_readiness(
                &profile_id,
                &credentials,
                CredentialReadinessSnapshot {
                    web: web_readiness,
                    access_key: CredentialReadiness::LoginRequired,
                },
            );
            return Err(Status::failed_precondition(
                "A previously verified Web account is required for generic access-key login.",
            ));
        };

        if has_access_key(&credentials) {
            match timeout(
                self.credential_timeout,
                self.credential_provider
                    .account_identity(credentials.clone(), CredentialKind::AccessKey),
            )
            .await
            {
                Ok(Ok(identity))
                    if identity.kind == CredentialKind::AccessKey
                        && identity.account_id == binding.account_id() =>
                {
                    self.set_readiness(
                        &profile_id,
                        &credentials,
                        CredentialReadinessSnapshot {
                            web: web_readiness,
                            access_key: CredentialReadiness::Ready,
                        },
                    );
                    let session = ready_session(
                        &session_id,
                        &profile_id,
                        LoginMethod::AccessKeyBrowser,
                        "Bilibili access-key credentials are already ready.",
                    );
                    self.insert_terminal_session(session.clone(), LoginMethod::AccessKeyBrowser)?;
                    reservation.disarm();
                    self.release_profile(&profile_id, &session_id);
                    return Ok(session);
                }
                Ok(Ok(_)) => {
                    self.set_readiness(
                        &profile_id,
                        &credentials,
                        CredentialReadinessSnapshot {
                            web: web_readiness,
                            access_key: CredentialReadiness::LoginRequired,
                        },
                    );
                    return Err(Status::failed_precondition(
                        "The saved access key belongs to a different Web account.",
                    ));
                }
                Ok(Err(CredentialProviderError::Rejected)) => {}
                Ok(Err(CredentialProviderError::Unavailable)) | Err(_) => {
                    self.set_readiness(
                        &profile_id,
                        &credentials,
                        CredentialReadinessSnapshot {
                            web: web_readiness,
                            access_key: CredentialReadiness::Unavailable,
                        },
                    );
                    return Err(Status::unavailable(
                        "Could not verify the saved access key. Please retry later.",
                    ));
                }
            }
        }

        let ticket = AccessKeyLoginConfig::biliplus(&server_origin)
            .and_then(|config| config.ticket())
            .map_err(|_| {
                Status::unavailable(
                    "Could not start Bilibili access-key login. Please retry later.",
                )
            })?;
        if ticket.callback_origin != server_origin
            || !valid_provider_url(&ticket.url)
            || ticket.message_origin != "https://www.biliplus.com"
        {
            return Err(Status::unavailable(
                "Could not start Bilibili access-key login. Please retry later.",
            ));
        }
        let capability = new_capability();
        let verification_uri =
            format!("{server_origin}{base_path}/login/bilibili/{session_id}#{capability}");
        let session = BilibiliLoginSession {
            id: session_id.clone(),
            profile_id: profile_id.clone(),
            method: crate::generated::tvos_net_player::v1::BilibiliLoginMethod::AccessKeyBrowser
                .into(),
            state: BilibiliLoginSessionState::Pending.into(),
            message: "Waiting for Bilibili authorization.".to_owned(),
            verification_uri,
            created_at: Some(current_timestamp()),
            expires_at: Some(timestamp_after(SESSION_TTL)),
        };
        let now = Instant::now();
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
                    credential_path: Some(path.clone()),
                    method: LoginMethod::AccessKeyBrowser,
                    ticket_key: None,
                    access_ticket: Some(ticket),
                    capability: Some(capability),
                    server_origin: Some(server_origin),
                    expected_account_id: Some(binding.account_id()),
                    expected_web_binding: Some(binding),
                    baseline: credential_baseline(&credentials, &secrets),
                    deadline: now + SESSION_TTL,
                    finishing: false,
                    completed_at: None,
                },
            );
        }
        self.set_readiness(
            &profile_id,
            &credentials,
            CredentialReadinessSnapshot {
                web: web_readiness,
                access_key: CredentialReadiness::Checking,
            },
        );
        let manager = self.clone();
        let expiry_id = session.id.clone();
        tokio::spawn(async move {
            sleep(SESSION_TTL).await;
            manager.expire_session(&expiry_id);
        });
        reservation.disarm();
        Ok(session)
    }

    pub(crate) fn browser_page(
        &self,
        session_id: &str,
        request_origin: &str,
    ) -> Result<String, Status> {
        let request_origin = normalize_origin(request_origin)
            .ok_or_else(|| Status::permission_denied("Login origin is not allowed."))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?;
        let now = Instant::now();
        expire_sessions(&mut state, now);
        prune_terminal_sessions(&mut state, now);
        let record = state
            .sessions
            .get(session_id)
            .filter(|record| {
                record.method == LoginMethod::AccessKeyBrowser
                    && record.public.state() == BilibiliLoginSessionState::Pending
                    && !record.finishing
                    && record.deadline > now
                    && record.server_origin.as_deref() == Some(request_origin.as_str())
            })
            .ok_or_else(|| Status::not_found("Bilibili login session not found."))?;
        let ticket = record
            .access_ticket
            .as_ref()
            .ok_or_else(|| Status::unavailable("Bilibili login page is unavailable."))?;
        render_browser_page(ticket)
    }

    pub(crate) async fn complete_browser_login(
        &self,
        session_id: &str,
        request_origin: &str,
        capability: &str,
        message_origin: &str,
        message: &str,
    ) -> Result<(), Status> {
        if message.len() > MAX_BROWSER_MESSAGE_BYTES {
            return Err(Status::invalid_argument("Invalid login completion."));
        }
        let request_origin = normalize_origin(request_origin)
            .ok_or_else(|| Status::permission_denied("Login origin is not allowed."))?;
        let (profile_id, path, ticket, expected_account_id, expected_binding, baseline) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Status::internal("Bilibili login session store is unavailable."))?;
            let now = Instant::now();
            expire_sessions(&mut state, now);
            let record = state
                .sessions
                .get(session_id)
                .ok_or_else(|| Status::not_found("Bilibili login session not found."))?;
            if record.method != LoginMethod::AccessKeyBrowser
                || record.public.state() != BilibiliLoginSessionState::Pending
                || record.finishing
                || record.deadline <= now
                || record.server_origin.as_deref() != Some(request_origin.as_str())
                || !record
                    .capability
                    .as_deref()
                    .is_some_and(|expected| constant_time_equal(expected, capability))
                || state
                    .active_profiles
                    .get(&record.public.profile_id)
                    .map(String::as_str)
                    != Some(session_id)
            {
                return Err(Status::permission_denied(
                    "Login completion is not authorized.",
                ));
            }
            let values = (
                record.public.profile_id.clone(),
                record.credential_path.clone().ok_or_else(|| {
                    Status::failed_precondition("Credential storage is unavailable.")
                })?,
                record
                    .access_ticket
                    .clone()
                    .ok_or_else(|| Status::unavailable("Login session is unavailable."))?,
                record.expected_account_id,
                record.expected_web_binding.clone(),
                record.baseline.clone(),
            );
            state
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| Status::not_found("Bilibili login session not found."))?
                .finishing = true;
            values
        };

        let completion_guard = BrowserCompletionGuard::new(self, &profile_id, session_id);
        let mut session_completed_by_worker = false;
        let result = async {
            if message_origin != ticket.message_origin {
                return Err(Status::permission_denied(
                    "Login completion is not authorized.",
                ));
            }
            let credentials = ticket
                .credentials_from_message(message_origin, message)
                .map_err(|_| Status::invalid_argument("Invalid login completion."))?;
            let identity = timeout(
                self.credential_timeout,
                self.credential_provider
                    .account_identity(credentials.credentials(), CredentialKind::AccessKey),
            )
            .await
            .map_err(|_| Status::unavailable("Could not verify the Bilibili account."))?
            .map_err(|error| match error {
                CredentialProviderError::Rejected => {
                    Status::failed_precondition("The Bilibili account requires a new login.")
                }
                CredentialProviderError::Unavailable => {
                    Status::unavailable("Could not verify the Bilibili account.")
                }
            })?;
            if identity.kind != CredentialKind::AccessKey
                || Some(identity.account_id) != expected_account_id
            {
                return Err(Status::failed_precondition(
                    "The authorization belongs to a different Web account.",
                ));
            }
            let manager = self.clone();
            let worker_session_id = session_id.to_owned();
            let worker_profile_id = profile_id.clone();
            let phase = completion_guard.phase();
            let outcome = tokio::task::spawn_blocking(move || {
                manager.commit_browser_access_key(BrowserAccessKeyCommit {
                    session_id: worker_session_id,
                    profile_id: worker_profile_id,
                    path,
                    baseline,
                    expected_binding,
                    credentials,
                    phase,
                })
            })
            .await
            .map_err(|_| Status::unavailable("Could not save Bilibili credentials."))?;
            match outcome {
                BrowserCommitOutcome::Completed {
                    result,
                    session_completed,
                } => {
                    session_completed_by_worker = true;
                    if !session_completed {
                        return Err(Status::aborted("Login session is no longer active."));
                    }
                    result.map_err(|error| match error {
                        CredentialCommitError::Changed => Status::aborted(
                            "Credential storage changed during login. Start a new session.",
                        ),
                        CredentialCommitError::Unavailable => {
                            Status::unavailable("Could not save Bilibili credentials.")
                        }
                    })?;
                }
                BrowserCommitOutcome::Expired => {
                    session_completed_by_worker = true;
                    return Err(Status::aborted("Login session is no longer active."));
                }
                BrowserCommitOutcome::Cancelled | BrowserCommitOutcome::Inactive => {
                    return Err(Status::aborted("Login session is no longer active."));
                }
                BrowserCommitOutcome::Unavailable => {
                    return Err(Status::unavailable("Could not save Bilibili credentials."));
                }
            }
            Ok(())
        }
        .await;

        let saved = result.is_ok();
        if !session_completed_by_worker
            && !self.complete_claimed_session(session_id, &profile_id, saved)
        {
            completion_guard.finish();
            return Err(Status::aborted("Login session is no longer active."));
        }
        completion_guard.finish();
        result
    }

    fn cancel_browser_completion(&self, session_id: &str, profile_id: &str) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let now = Instant::now();
        if expire_browser_completion(&mut state, session_id, profile_id, now) {
            return;
        }
        if state.active_profiles.get(profile_id).map(String::as_str) != Some(session_id) {
            return;
        }
        if let Some(record) = state.sessions.get_mut(session_id)
            && record.public.profile_id == profile_id
            && record.method == LoginMethod::AccessKeyBrowser
            && record.public.state() == BilibiliLoginSessionState::Pending
            && record.finishing
        {
            record.finishing = false;
        }
    }

    fn claim_browser_commit(
        &self,
        session_id: &str,
        profile_id: &str,
        phase: &AtomicU8,
    ) -> BrowserCommitClaim {
        let Ok(mut state) = self.state.lock() else {
            return BrowserCommitClaim::Unavailable;
        };
        if phase.load(std::sync::atomic::Ordering::Acquire) != BROWSER_COMPLETION_ACTIVE {
            return BrowserCommitClaim::Cancelled;
        }
        let now = Instant::now();
        if expire_browser_completion(&mut state, session_id, profile_id, now) {
            phase.store(
                BROWSER_COMPLETION_FINISHED,
                std::sync::atomic::Ordering::Release,
            );
            return BrowserCommitClaim::Expired;
        }
        let owns_profile =
            state.active_profiles.get(profile_id).map(String::as_str) == Some(session_id);
        let is_finishing = state.sessions.get(session_id).is_some_and(|record| {
            record.method == LoginMethod::AccessKeyBrowser
                && record.public.profile_id == profile_id
                && record.public.state() == BilibiliLoginSessionState::Pending
                && record.finishing
                && record.deadline > now
        });
        if !owns_profile || !is_finishing {
            return BrowserCommitClaim::Inactive;
        }

        // This transition linearizes cancellation against the non-abortable blocking write.
        match phase.compare_exchange(
            BROWSER_COMPLETION_ACTIVE,
            BROWSER_COMPLETION_COMMITTING,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => BrowserCommitClaim::Claimed,
            Err(BROWSER_COMPLETION_CANCELLED) => BrowserCommitClaim::Cancelled,
            Err(_) => BrowserCommitClaim::Inactive,
        }
    }

    fn commit_browser_access_key(&self, commit: BrowserAccessKeyCommit) -> BrowserCommitOutcome {
        let BrowserAccessKeyCommit {
            session_id,
            profile_id,
            path,
            baseline,
            expected_binding,
            credentials,
            phase,
        } = commit;
        match self.claim_browser_commit(&session_id, &profile_id, &phase) {
            BrowserCommitClaim::Claimed => {}
            BrowserCommitClaim::Cancelled => return BrowserCommitOutcome::Cancelled,
            BrowserCommitClaim::Expired => return BrowserCommitOutcome::Expired,
            BrowserCommitClaim::Inactive => return BrowserCommitOutcome::Inactive,
            BrowserCommitClaim::Unavailable => return BrowserCommitOutcome::Unavailable,
        }

        let mut worker = BrowserCommitWorkerGuard::new(
            self.clone(),
            profile_id.clone(),
            session_id.clone(),
            phase,
        );
        let result = persist_access_key(
            &path,
            &profile_id,
            &baseline,
            expected_binding.as_ref(),
            credentials,
        );
        if result.is_ok()
            && let Ok(loaded) = load_profile_bundle(&path, &profile_id)
        {
            let web_readiness = self
                .state
                .lock()
                .ok()
                .and_then(|state| {
                    state
                        .readiness
                        .get(&profile_id)
                        .map(|record| record.snapshot.web)
                })
                .unwrap_or(CredentialReadiness::Unknown);
            self.set_readiness(
                &profile_id,
                &loaded.credentials,
                CredentialReadinessSnapshot {
                    web: web_readiness,
                    access_key: CredentialReadiness::Ready,
                },
            );
        }
        let session_completed = worker.finish(result.is_ok());
        BrowserCommitOutcome::Completed {
            result,
            session_completed,
        }
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
                let Some(key) = record.ticket_key.clone() else {
                    return;
                };
                (key, record.public.profile_id.clone())
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
                            path.clone(),
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
                        self.verify_preserved_access_key_after_web_login(&profile_id, &path)
                            .await;
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
        let identity = match timeout(
            self.credential_timeout,
            self.credential_provider.account_identity(
                Credentials::default().with_cookie(cookie.clone()),
                CredentialKind::Cookie,
            ),
        )
        .await
        {
            Ok(Ok(identity)) if identity.kind == CredentialKind::Cookie => identity,
            _ => return false,
        };

        let (baseline, expected_binding, expected_account_id) = {
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
                || record
                    .expected_account_id
                    .is_some_and(|expected| expected != identity.account_id)
            {
                return false;
            }
            record.finishing = true;
            (
                record.baseline.clone(),
                record.expected_web_binding.clone(),
                record.expected_account_id,
            )
        };
        if expected_account_id.is_some_and(|expected| expected != identity.account_id) {
            return false;
        }

        let store_profile_id = profile_id.clone();
        let store_path = path.clone();
        let saved = tokio::task::spawn_blocking(move || {
            persist_web_login(
                &store_path,
                &store_profile_id,
                &baseline,
                expected_binding.as_ref(),
                &cookie,
                refresh_token.as_deref(),
                identity.account_id,
            )
        })
        .await
        .is_ok_and(|result| result);

        if saved && let Ok(loaded) = load_profile_bundle(&path, &profile_id) {
            self.set_readiness(
                &profile_id,
                &loaded.credentials,
                CredentialReadinessSnapshot {
                    web: CredentialReadiness::Ready,
                    access_key: if has_access_key(&loaded.credentials) {
                        CredentialReadiness::Unknown
                    } else {
                        CredentialReadiness::Missing
                    },
                },
            );
        }
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
            let method = record.method;
            record.public.state = if saved {
                BilibiliLoginSessionState::Ready.into()
            } else {
                BilibiliLoginSessionState::Error.into()
            };
            record.public.message = if saved {
                match method {
                    LoginMethod::WebQr => "Bilibili Web login completed.",
                    LoginMethod::AccessKeyBrowser => "Bilibili access-key login completed.",
                }
                .to_owned()
            } else {
                "Login could not be saved. Check server credential storage and retry.".to_owned()
            };
            clear_private_session(record);
            record.finishing = false;
            record.completed_at = Some(Instant::now());
            if !saved && let Some(readiness) = state.readiness.get_mut(profile_id) {
                match method {
                    LoginMethod::WebQr
                        if readiness.snapshot.web == CredentialReadiness::Checking =>
                    {
                        readiness.snapshot.web = CredentialReadiness::Unavailable;
                    }
                    LoginMethod::AccessKeyBrowser
                        if readiness.snapshot.access_key == CredentialReadiness::Checking =>
                    {
                        readiness.snapshot.access_key = CredentialReadiness::Unavailable;
                    }
                    _ => {}
                }
            }
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
            let mut terminal = false;
            if let Some(record) = state.sessions.get_mut(session_id)
                && owns_profile
                && record.public.state() == BilibiliLoginSessionState::Pending
                && !record.finishing
            {
                record.public.state = state_value.into();
                record.public.message = message.to_owned();
                clear_private_session(record);
                record.completed_at = Some(Instant::now());
                terminal = true;
            }
            if terminal {
                if let Some(readiness) = state.readiness.get_mut(profile_id) {
                    let terminal_readiness = if state_value == BilibiliLoginSessionState::Expired {
                        CredentialReadiness::LoginRequired
                    } else {
                        CredentialReadiness::Unavailable
                    };
                    if readiness.snapshot.web == CredentialReadiness::Checking {
                        readiness.snapshot.web = terminal_readiness;
                    }
                    if readiness.snapshot.access_key == CredentialReadiness::Checking {
                        readiness.snapshot.access_key = terminal_readiness;
                    }
                }
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

fn binding_from_path(
    path: &Path,
    profile_id: &str,
) -> Result<Option<VerifiedWebBinding>, BindingError> {
    bilibili_credential_bindings::lookup_profile(path, profile_id)
}

fn publish_verified_binding(
    path: &Path,
    profile_id: &str,
    credentials: &Credentials,
    secrets: &CredentialProfileSecrets,
    expected: Option<&VerifiedWebBinding>,
    account_id: u64,
) -> Result<VerifiedWebBinding, BindingError> {
    let profile = profile_id.to_owned();
    let cookie = credentials
        .cookie
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or(BindingError::Changed)?
        .to_owned();
    let baseline = credential_baseline(credentials, secrets);
    let mut profile_changed = false;
    let profile_result = CredentialStore::new(path.to_path_buf())
        .update_profiles(|profiles| {
            let current = profiles.profile(&profile).unwrap_or_default();
            let current_secrets = profiles.profile_secrets(&profile)?;
            if !web_baseline_matches(&current, &current_secrets, &baseline) {
                profile_changed = true;
                return Err(bbdown_core::Error::InvalidInput(
                    "credential profile changed during verification".to_owned(),
                ));
            }
            Ok(())
        })
        .map_err(|_| {
            if profile_changed {
                BindingError::Changed
            } else {
                BindingError::Unavailable
            }
        });
    profile_result?;
    bilibili_credential_bindings::replace(path, &profile, &cookie, account_id, expected)
}

fn persist_refreshed_web(
    path: &Path,
    profile_id: &str,
    baseline: &CredentialBaseline,
    expected_binding: &VerifiedWebBinding,
    cookie: &str,
    refresh_token: Option<&str>,
    account_id: u64,
) -> Result<VerifiedWebBinding, BindingError> {
    let profile = profile_id.to_owned();
    let cookie = cookie.to_owned();
    let refresh_token = refresh_token.map(str::to_owned);
    let mut profile_changed = false;
    let profile_result = CredentialStore::new(path.to_path_buf())
        .update_profiles(|profiles| {
            let mut current = profiles.profile(&profile).unwrap_or_default();
            let mut secrets = profiles.profile_secrets(&profile)?;
            if !web_baseline_matches(&current, &secrets, baseline) {
                profile_changed = true;
                return Err(bbdown_core::Error::InvalidInput(
                    "credential profile changed during refresh".to_owned(),
                ));
            }
            current.cookie = Some(cookie.clone());
            profiles.set_profile(&profile, current)?;
            secrets.set_cookie(
                refresh_token
                    .as_ref()
                    .map_or_else(CredentialRefreshSecret::default, |value| {
                        CredentialRefreshSecret::default().with_refresh_token(value.clone())
                    }),
            );
            profiles.set_profile_secrets(&profile, secrets)?;
            Ok(())
        })
        .map_err(|_| {
            if profile_changed {
                BindingError::Changed
            } else {
                BindingError::Unavailable
            }
        });
    profile_result?;
    // A failed sidecar publication leaves the committed cookie unbound and unusable until reverified.
    bilibili_credential_bindings::replace(
        path,
        &profile,
        &cookie,
        account_id,
        Some(expected_binding),
    )
}

fn persist_web_login(
    path: &Path,
    profile_id: &str,
    baseline: &CredentialBaseline,
    expected_binding: Option<&VerifiedWebBinding>,
    cookie: &str,
    refresh_token: Option<&str>,
    account_id: u64,
) -> bool {
    let profile = profile_id.to_owned();
    let cookie = cookie.to_owned();
    let refresh_token = refresh_token.map(str::to_owned);
    let persisted = CredentialStore::new(path.to_path_buf())
        .update_profiles(|profiles| {
            let mut current = profiles.profile(&profile).unwrap_or_default();
            let mut secrets = profiles.profile_secrets(&profile)?;
            if !web_baseline_matches(&current, &secrets, baseline) {
                return Err(bbdown_core::Error::InvalidInput(
                    "credential profile changed during login".to_owned(),
                ));
            }
            current.cookie = Some(cookie.clone());
            profiles.set_profile(&profile, current)?;
            secrets.set_cookie(
                refresh_token
                    .as_ref()
                    .map_or_else(CredentialRefreshSecret::default, |value| {
                        CredentialRefreshSecret::default().with_refresh_token(value.clone())
                    }),
            );
            profiles.set_profile_secrets(&profile, secrets)
        })
        .is_ok();
    if !persisted {
        return false;
    }
    // Publish identity only after the credential store accepted its content baseline.
    bilibili_credential_bindings::replace(path, &profile, &cookie, account_id, expected_binding)
        .is_ok()
}

fn persist_access_key(
    path: &Path,
    profile_id: &str,
    baseline: &CredentialBaseline,
    expected_binding: Option<&VerifiedWebBinding>,
    credentials: AccessKeyLoginCredentials,
) -> Result<(), CredentialCommitError> {
    let profile = profile_id.to_owned();
    let mut commit_error = None;
    CredentialStore::new(path.to_path_buf())
        .update_profiles(|profiles| {
            let mut current = profiles.profile(&profile).unwrap_or_default();
            let secrets = profiles.profile_secrets(&profile)?;
            if !credential_baseline_matches(&current, &secrets, baseline) {
                commit_error = Some(CredentialCommitError::Changed);
                return Err(bbdown_core::Error::InvalidInput(
                    "credential profile changed during login".to_owned(),
                ));
            }
            let current_binding = binding_from_path(path, &profile).map_err(|_| {
                commit_error = Some(CredentialCommitError::Unavailable);
                bbdown_core::Error::InvalidInput("verified binding is unavailable".to_owned())
            })?;
            if current_binding.as_ref() != expected_binding {
                commit_error = Some(CredentialCommitError::Changed);
                return Err(bbdown_core::Error::InvalidInput(
                    "verified binding changed during login".to_owned(),
                ));
            }
            current.access_key = Some(credentials.access_key.clone());
            profiles.set_profile(&profile, current)?;
            let mut secrets = profiles.profile_secrets(&profile)?;
            let access_key_secret = credentials
                .refresh_token
                .as_deref()
                .filter(|token| !token.trim().is_empty())
                .map_or_else(AccessKeyProviderSecret::default, |token| {
                    AccessKeyProviderSecret::default().with_refresh_token(token)
                });
            secrets.set_access_key_provider(AccessKeyProvider::BalhBiliplus, access_key_secret);
            profiles.set_profile_secrets(&profile, secrets)
        })
        .map_err(|_| commit_error.unwrap_or(CredentialCommitError::Unavailable))
}

fn credential_baseline(
    credentials: &Credentials,
    secrets: &CredentialProfileSecrets,
) -> CredentialBaseline {
    CredentialBaseline {
        web_cookie: credential_fingerprint(credentials.cookie.as_deref()),
        web_refresh: credential_fingerprint(
            secrets
                .cookie()
                .and_then(|secret| secret.refresh_token.as_deref()),
        ),
        access_key: credential_fingerprint(credentials.access_key.as_deref()),
        access_secrets: serde_json::to_vec(&secrets.access_key)
            .ok()
            .map(|value| bytes_fingerprint(&value)),
    }
}

fn web_baseline_matches(
    credentials: &Credentials,
    secrets: &CredentialProfileSecrets,
    baseline: &CredentialBaseline,
) -> bool {
    baseline.web_cookie == credential_fingerprint(credentials.cookie.as_deref())
        && baseline.web_refresh
            == credential_fingerprint(
                secrets
                    .cookie()
                    .and_then(|secret| secret.refresh_token.as_deref()),
            )
}

fn credential_baseline_matches(
    credentials: &Credentials,
    secrets: &CredentialProfileSecrets,
    baseline: &CredentialBaseline,
) -> bool {
    web_baseline_matches(credentials, secrets, baseline)
        && baseline.access_key == credential_fingerprint(credentials.access_key.as_deref())
        && baseline.access_secrets
            == serde_json::to_vec(&secrets.access_key)
                .ok()
                .map(|value| bytes_fingerprint(&value))
}

fn credential_fingerprint(value: Option<&str>) -> Option<String> {
    value.map(|value| bytes_fingerprint(value.as_bytes()))
}

fn credential_bundle_fingerprint(
    credentials: &Credentials,
    secrets: &CredentialProfileSecrets,
) -> Option<String> {
    serde_json::to_vec(&(credentials, secrets))
        .ok()
        .map(|bundle| bytes_fingerprint(&bundle))
}

fn bytes_fingerprint(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn load_profile_bundle(path: &Path, profile_id: &str) -> Result<ProfileCredentials, Status> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(Status::failed_precondition(
            "BBDown credential storage directory is unavailable or not writable.",
        ));
    }
    let profiles = CredentialStore::new(path.to_path_buf())
        .load_profiles()
        .map_err(|_| Status::unavailable("Could not read BBDown credential storage."))?;
    let credentials = profiles.profile(profile_id).unwrap_or_default();
    let secrets = profiles
        .profile_secrets(profile_id)
        .map_err(|_| Status::unavailable("Could not read BBDown credential storage."))?;
    Ok(ProfileCredentials {
        credentials,
        secrets,
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

fn valid_provider_url(uri: &str) -> bool {
    let Ok(parsed) = url::Url::parse(uri) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("www.biliplus.com")
        && parsed.path() == "/login"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed
            .query_pairs()
            .any(|(key, value)| key == "balh_auth" && value == "1")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserPageData<'a> {
    provider_url: &'a str,
    message_origin: &'a str,
}

fn render_browser_page(ticket: &AccessKeyLoginTicket) -> Result<String, Status> {
    let data = serde_json::to_string(&BrowserPageData {
        provider_url: &ticket.url,
        message_origin: &ticket.message_origin,
    })
    .map_err(|_| Status::unavailable("Bilibili login page is unavailable."))?
    .replace('&', "\\u0026")
    .replace('<', "\\u003c")
    .replace('>', "\\u003e")
    .replace('\u{2028}', "\\u2028")
    .replace('\u{2029}', "\\u2029");
    let template = include_str!("../assets/bilibili_access_key_handoff.html");
    if template.matches("__HANDOFF_DATA_JSON__").count() != 1 {
        return Err(Status::unavailable("Bilibili login page is unavailable."));
    }
    Ok(template.replace("__HANDOFF_DATA_JSON__", &data))
}

fn parse_login_base_uri(value: &str) -> Result<(String, String), Status> {
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(Status::failed_precondition(
            "A trusted Bilibili login origin is unavailable.",
        ));
    }
    let parsed = url::Url::parse(value).map_err(|_| {
        Status::failed_precondition("A trusted Bilibili login origin is unavailable.")
    })?;
    let host = parsed.host_str().unwrap_or_default();
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || matches!(host, "0.0.0.0" | "::")
        || parsed.path().to_ascii_lowercase().contains("%2e")
    {
        return Err(Status::failed_precondition(
            "A trusted Bilibili login origin is unavailable.",
        ));
    }
    let origin = parsed.origin().ascii_serialization();
    let base_path = parsed.path().trim_end_matches('/').to_owned();
    Ok((origin, base_path))
}

fn normalize_origin(value: &str) -> Option<String> {
    let parsed = url::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let origin = parsed.origin().ascii_serialization();
    (origin == value).then_some(origin)
}

fn new_capability() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn ready_session(
    session_id: &str,
    profile_id: &str,
    method: LoginMethod,
    message: &str,
) -> BilibiliLoginSession {
    BilibiliLoginSession {
        id: session_id.to_owned(),
        profile_id: profile_id.to_owned(),
        method: match method {
            LoginMethod::WebQr => crate::generated::tvos_net_player::v1::BilibiliLoginMethod::WebQr,
            LoginMethod::AccessKeyBrowser => {
                crate::generated::tvos_net_player::v1::BilibiliLoginMethod::AccessKeyBrowser
            }
        }
        .into(),
        state: BilibiliLoginSessionState::Ready.into(),
        message: message.to_owned(),
        verification_uri: String::new(),
        created_at: Some(current_timestamp()),
        expires_at: Some(timestamp_after(SESSION_TTL)),
    }
}

fn has_cookie(credentials: &Credentials) -> bool {
    credentials
        .cookie
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
}

fn has_access_key(credentials: &Credentials) -> bool {
    credentials
        .access_key
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
}

fn readiness_status(readiness: CredentialReadiness) -> Status {
    match readiness {
        CredentialReadiness::LoginRequired | CredentialReadiness::Missing => {
            Status::failed_precondition("Bilibili credentials require a server-owned login.")
        }
        CredentialReadiness::Unavailable | CredentialReadiness::Unknown => {
            Status::unavailable("Could not verify Bilibili credentials. Please retry later.")
        }
        CredentialReadiness::Checking | CredentialReadiness::Ready => {
            Status::failed_precondition("A Bilibili credential operation is already active.")
        }
    }
}

fn clear_private_session(record: &mut SessionRecord) {
    record.public.verification_uri.clear();
    record.credential_path = None;
    record.ticket_key = None;
    record.access_ticket = None;
    record.capability = None;
    record.server_origin = None;
    record.expected_account_id = None;
    record.expected_web_binding = None;
    record.baseline = CredentialBaseline::default();
}

fn expire_browser_completion(
    state: &mut LoginState,
    session_id: &str,
    profile_id: &str,
    now: Instant,
) -> bool {
    if state.active_profiles.get(profile_id).map(String::as_str) != Some(session_id) {
        return false;
    }
    let expired = {
        let Some(record) = state.sessions.get_mut(session_id) else {
            return false;
        };
        if record.public.profile_id != profile_id
            || record.method != LoginMethod::AccessKeyBrowser
            || record.public.state() != BilibiliLoginSessionState::Pending
            || !record.finishing
            || now < record.deadline
        {
            return false;
        }
        record.public.state = BilibiliLoginSessionState::Expired.into();
        record.public.message = "The login session expired.".to_owned();
        clear_private_session(record);
        record.finishing = false;
        record.completed_at = Some(now);
        true
    };
    if expired {
        if let Some(readiness) = state.readiness.get_mut(profile_id)
            && readiness.snapshot.access_key == CredentialReadiness::Checking
        {
            readiness.snapshot.access_key = CredentialReadiness::LoginRequired;
        }
        state.active_profiles.remove(profile_id);
    }
    expired
}

fn expire_sessions(state: &mut LoginState, now: Instant) {
    let mut released = Vec::new();
    for record in state.sessions.values_mut() {
        if record.public.state() == BilibiliLoginSessionState::Pending
            && !record.finishing
            && now >= record.deadline
        {
            record.public.state = BilibiliLoginSessionState::Expired.into();
            record.public.message = "The login session expired.".to_owned();
            clear_private_session(record);
            record.completed_at = Some(now);
            released.push((
                record.public.profile_id.clone(),
                record.public.id.clone(),
                record.method,
            ));
        }
    }
    for (profile_id, session_id, method) in released {
        if state.active_profiles.get(&profile_id).map(String::as_str) == Some(&session_id) {
            state.active_profiles.remove(&profile_id);
        }
        if let Some(readiness) = state.readiness.get_mut(&profile_id) {
            match method {
                LoginMethod::WebQr if readiness.snapshot.web == CredentialReadiness::Checking => {
                    readiness.snapshot.web = CredentialReadiness::LoginRequired;
                }
                LoginMethod::AccessKeyBrowser
                    if readiness.snapshot.access_key == CredentialReadiness::Checking =>
                {
                    readiness.snapshot.access_key = CredentialReadiness::LoginRequired;
                }
                _ => {}
            }
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
    use std::{collections::HashMap, sync::Mutex};

    use bbdown_core::{
        CredentialKind, CredentialProfileSecrets, CredentialRefreshSecret, CredentialStore,
        Credentials,
    };
    use tonic::Code;

    use super::*;
    use crate::bilibili_credentials::{VerifiedIdentity, WebCookieRefreshResult};

    // Credential fixture IDs from joey-private-v3: bearer-a/b, access-a/b, refresh-a/b.
    const WEB_COOKIE_A: &str = "codex_synth_v1_bearer_a";
    const WEB_COOKIE_B: &str = "JoeyPrivateV3BearerSlotB7Q9M3X5";
    const ACCESS_KEY_A: &str = "codex_synth_v1_access_a";
    const ACCESS_KEY_B: &str = "codex_synth_v1_access_b";
    const REFRESH_TOKEN_A: &str = "codex_synth_v1_refresh_a";
    const REFRESH_TOKEN_B: &str = "codex_synth_v1_refresh_b";

    #[derive(Clone, Copy)]
    enum FakeIdentityOutcome {
        Account(u64),
        Rejected,
        Unavailable,
    }

    #[derive(Clone, Copy)]
    enum FakeRefreshOutcome {
        NoRefresh,
        Refreshed {
            cookie: &'static str,
            refresh_token: &'static str,
        },
        Rejected,
        Unavailable,
    }

    struct FakeCredentialProvider {
        identities: Mutex<HashMap<String, FakeIdentityOutcome>>,
        refresh: Mutex<FakeRefreshOutcome>,
        refresh_calls: AtomicUsize,
        identity_gate: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
        access_key_identity_gate:
            Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    }

    impl Default for FakeCredentialProvider {
        fn default() -> Self {
            Self {
                identities: Mutex::new(HashMap::new()),
                refresh: Mutex::new(FakeRefreshOutcome::NoRefresh),
                refresh_calls: AtomicUsize::new(0),
                identity_gate: Mutex::new(None),
                access_key_identity_gate: Mutex::new(None),
            }
        }
    }

    impl FakeCredentialProvider {
        fn set_identity(
            &self,
            kind: CredentialKind,
            credential: &str,
            outcome: FakeIdentityOutcome,
        ) {
            let mut identities = self.identities.lock().expect("identity overrides");
            identities.insert(identity_key(kind, credential), outcome);
        }

        fn set_refresh(&self, outcome: FakeRefreshOutcome) {
            *self.refresh.lock().expect("refresh outcome") = outcome;
        }

        fn pause_next_identity(&self) -> Arc<tokio::sync::Notify> {
            self.pause_next_identity_with_release().0
        }

        fn pause_next_identity_with_release(
            &self,
        ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
            let started = Arc::new(tokio::sync::Notify::new());
            let resume = Arc::new(tokio::sync::Notify::new());
            *self.identity_gate.lock().expect("identity gate") =
                Some((started.clone(), resume.clone()));
            (started, resume)
        }

        fn pause_next_access_key_identity_with_release(
            &self,
        ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
            let started = Arc::new(tokio::sync::Notify::new());
            let resume = Arc::new(tokio::sync::Notify::new());
            *self
                .access_key_identity_gate
                .lock()
                .expect("access-key identity gate") = Some((started.clone(), resume.clone()));
            (started, resume)
        }
    }

    impl CredentialProvider for FakeCredentialProvider {
        fn account_identity(
            &self,
            credentials: Credentials,
            kind: CredentialKind,
        ) -> crate::bilibili_credentials::CredentialFuture<VerifiedIdentity> {
            let material = match kind {
                CredentialKind::Cookie => credentials.cookie,
                CredentialKind::AccessKey => credentials.access_key,
                CredentialKind::TvAccessKey => credentials.tv_access_key,
            }
            .unwrap_or_default();
            let outcome = self
                .identities
                .lock()
                .expect("identity overrides")
                .get(&identity_key(kind, &material))
                .copied()
                .unwrap_or(FakeIdentityOutcome::Account(31_001));
            let access_key_gate = if kind == CredentialKind::AccessKey {
                self.access_key_identity_gate
                    .lock()
                    .expect("access-key identity gate")
                    .take()
            } else {
                None
            };
            let gate = access_key_gate
                .or_else(|| self.identity_gate.lock().expect("identity gate").take());
            Box::pin(async move {
                if let Some((started, resume)) = gate {
                    started.notify_one();
                    resume.notified().await;
                }
                match outcome {
                    FakeIdentityOutcome::Account(account_id) => {
                        Ok(VerifiedIdentity { kind, account_id })
                    }
                    FakeIdentityOutcome::Rejected => Err(CredentialProviderError::Rejected),
                    FakeIdentityOutcome::Unavailable => Err(CredentialProviderError::Unavailable),
                }
            })
        }

        fn refresh_web_cookie(
            &self,
            cookie: String,
            refresh_token: String,
        ) -> crate::bilibili_credentials::CredentialFuture<WebCookieRefreshResult> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            let outcome = *self.refresh.lock().expect("refresh outcome");
            Box::pin(async move {
                match outcome {
                    FakeRefreshOutcome::NoRefresh => Ok(WebCookieRefreshResult {
                        cookie,
                        refresh_token,
                        refreshed: false,
                    }),
                    FakeRefreshOutcome::Refreshed {
                        cookie,
                        refresh_token,
                    } => Ok(WebCookieRefreshResult {
                        cookie: cookie.to_owned(),
                        refresh_token: refresh_token.to_owned(),
                        refreshed: true,
                    }),
                    FakeRefreshOutcome::Rejected => Err(CredentialProviderError::Rejected),
                    FakeRefreshOutcome::Unavailable => Err(CredentialProviderError::Unavailable),
                }
            })
        }
    }

    fn identity_key(kind: CredentialKind, credential: &str) -> String {
        let kind = match kind {
            CredentialKind::Cookie => "cookie",
            CredentialKind::AccessKey => "access-key",
            CredentialKind::TvAccessKey => "tv-access-key",
        };
        format!("{kind}:{credential}")
    }

    struct FakeProvider {
        creates: AtomicUsize,
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
            self.creates.fetch_add(1, Ordering::SeqCst);
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
        manager_with_provider(result, Arc::new(FakeCredentialProvider::default())).0
    }

    fn manager_with_provider(
        result: PollResult,
        credential_provider: Arc<FakeCredentialProvider>,
    ) -> (BilibiliLoginManager, Arc<FakeProvider>) {
        let web_provider = Arc::new(FakeProvider {
            creates: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
            result,
        });
        let manager =
            BilibiliLoginManager::with_providers(web_provider.clone(), credential_provider);
        (manager, web_provider)
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

    fn seed_web_profile(path: &Path, cookie: &str, refresh_token: Option<&str>) {
        CredentialStore::new(path.to_path_buf())
            .update_profiles(|profiles| {
                profiles.set_profile(
                    "default",
                    Credentials::default().with_cookie(cookie.to_owned()),
                )?;
                let mut secrets = CredentialProfileSecrets::default();
                if let Some(refresh_token) = refresh_token {
                    secrets.set_cookie(
                        CredentialRefreshSecret::default()
                            .with_refresh_token(refresh_token.to_owned()),
                    );
                }
                profiles.set_profile_secrets("default", secrets)
            })
            .expect("seed Web credential profile");
    }

    fn seed_profile_with_preserved_access_key(path: &Path) {
        CredentialStore::new(path.to_path_buf())
            .update_profiles(|profiles| {
                profiles.set_profile(
                    "default",
                    Credentials::default().with_access_key(ACCESS_KEY_A.to_owned()),
                )?;
                profiles.set_profile("other", Credentials::default().with_cookie(WEB_COOKIE_B))?;
                let mut secrets = CredentialProfileSecrets::default();
                secrets.set_access_key_provider(
                    AccessKeyProvider::BalhBiliplus,
                    AccessKeyProviderSecret::default()
                        .with_refresh_token(REFRESH_TOKEN_B.to_owned()),
                );
                profiles.set_profile_secrets("default", secrets)
            })
            .expect("seed preserved access key");
    }

    fn successful_web_login_manager(
        credential_provider: Arc<FakeCredentialProvider>,
    ) -> BilibiliLoginManager {
        manager_with_provider(
            PollResult::Succeeded {
                credentials: Credentials::default().with_cookie(WEB_COOKIE_A.to_owned()),
                refresh_token: Some(REFRESH_TOKEN_A.to_owned()),
            },
            credential_provider,
        )
        .0
    }

    async fn wait_for_access_readiness(
        manager: &BilibiliLoginManager,
        path: &Path,
        expected: CredentialReadiness,
    ) -> ProfileCredentials {
        for _ in 0..200 {
            let loaded = load_profile_bundle(path, "default").expect("profile remains readable");
            if manager.readiness("default", &loaded.credentials).access_key == expected {
                return loaded;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("access-key readiness did not reach the expected state");
    }

    async fn wait_for_profile_release(manager: &BilibiliLoginManager, profile_id: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !manager
                    .state
                    .lock()
                    .expect("state lock")
                    .active_profiles
                    .contains_key(profile_id)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("profile operation is released");
    }

    async fn start_successful_web_login(
        manager: &BilibiliLoginManager,
        path: &Path,
    ) -> BilibiliLoginSession {
        let session = manager
            .start("default".to_owned(), Some(path.to_path_buf()))
            .await
            .expect("Web QR session starts");
        let terminal = wait_terminal(manager, &session.id).await;
        assert_eq!(BilibiliLoginSessionState::Ready, terminal.state());
        terminal
    }

    #[tokio::test]
    async fn web_login_verifies_and_preserves_existing_access_key_without_waiting_for_maintenance()
    {
        let (_temp, path) = temp_store();
        seed_profile_with_preserved_access_key(&path);
        let manager = successful_web_login_manager(Arc::new(FakeCredentialProvider::default()));

        let session = start_successful_web_login(&manager, &path).await;
        let loaded = wait_for_access_readiness(&manager, &path, CredentialReadiness::Ready).await;

        assert_eq!(BilibiliLoginSessionState::Ready, session.state());
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
        assert_eq!(Some(ACCESS_KEY_A), loaded.credentials.access_key.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_B),
            loaded
                .secrets
                .access_key_provider(AccessKeyProvider::BalhBiliplus)
                .and_then(|secret| secret.refresh_token.as_deref())
        );
        assert_eq!(
            Some(WEB_COOKIE_B),
            CredentialStore::new(path)
                .load_profiles()
                .expect("profiles remain")
                .profile("other")
                .expect("other profile remains")
                .cookie
                .as_deref()
        );
    }

    #[tokio::test]
    async fn web_login_marks_rejected_preserved_access_key_login_required() {
        let (_temp, path) = temp_store();
        seed_profile_with_preserved_access_key(&path);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.set_identity(
            CredentialKind::AccessKey,
            ACCESS_KEY_A,
            FakeIdentityOutcome::Rejected,
        );
        let manager = successful_web_login_manager(credential_provider);

        start_successful_web_login(&manager, &path).await;
        let loaded =
            wait_for_access_readiness(&manager, &path, CredentialReadiness::LoginRequired).await;

        assert_eq!(Some(ACCESS_KEY_A), loaded.credentials.access_key.as_deref());
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
    }

    #[tokio::test]
    async fn web_login_keeps_preserved_access_key_unavailable_when_provider_fails() {
        let (_temp, path) = temp_store();
        seed_profile_with_preserved_access_key(&path);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.set_identity(
            CredentialKind::AccessKey,
            ACCESS_KEY_A,
            FakeIdentityOutcome::Unavailable,
        );
        let manager = successful_web_login_manager(credential_provider);

        start_successful_web_login(&manager, &path).await;
        let loaded =
            wait_for_access_readiness(&manager, &path, CredentialReadiness::Unavailable).await;

        assert_eq!(Some(ACCESS_KEY_A), loaded.credentials.access_key.as_deref());
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
    }

    #[tokio::test]
    async fn stale_web_login_access_key_probe_cannot_overwrite_newer_profile_readiness() {
        let (_temp, path) = temp_store();
        seed_profile_with_preserved_access_key(&path);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        let (started, resume) = credential_provider.pause_next_access_key_identity_with_release();
        let manager = successful_web_login_manager(credential_provider);

        let session = start_successful_web_login(&manager, &path).await;
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("targeted access-key verification starts");
        assert_eq!(
            BilibiliLoginSessionState::Ready,
            manager
                .get(&session.id)
                .expect("completed Web session remains queryable")
                .state()
        );
        let checking = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert_eq!(
            CredentialReadiness::Checking,
            manager
                .readiness("default", &checking.credentials)
                .access_key
        );

        CredentialStore::new(path.clone())
            .update_profiles(|profiles| {
                let mut current = profiles.profile("default").unwrap_or_default();
                current.access_key = Some(ACCESS_KEY_B.to_owned());
                profiles.set_profile("default", current)
            })
            .expect("replace the profile key during verification");
        let observed_binding =
            match bilibili_credential_bindings::lookup(&path, "default", WEB_COOKIE_A)
                .expect("binding remains readable")
            {
                BindingLookup::Matches(binding) => binding,
                _ => panic!("Web binding is present after login"),
            };
        bilibili_credential_bindings::replace(
            &path,
            "default",
            WEB_COOKIE_A,
            31_002,
            Some(&observed_binding),
        )
        .expect("replace the binding during verification");
        let changed = load_profile_bundle(&path, "default").expect("updated profile remains");
        manager.set_readiness(
            "default",
            &changed.credentials,
            CredentialReadinessSnapshot {
                web: CredentialReadiness::Ready,
                access_key: CredentialReadiness::Ready,
            },
        );

        resume.notify_one();
        wait_for_profile_release(&manager, "default").await;

        let current = load_profile_bundle(&path, "default").expect("updated profile remains");
        assert_eq!(BilibiliLoginSessionState::Ready, session.state());
        assert_eq!(
            Some(ACCESS_KEY_B),
            current.credentials.access_key.as_deref()
        );
        assert_eq!(
            CredentialReadiness::Ready,
            manager
                .readiness("default", &current.credentials)
                .access_key
        );
        assert_eq!(
            CredentialReadiness::Ready,
            manager.readiness("default", &current.credentials).web
        );
        assert!(matches!(
            bilibili_credential_bindings::lookup(&path, "default", WEB_COOKIE_A),
            Ok(BindingLookup::Matches(binding)) if binding.account_id() == 31_002
        ));
    }

    #[tokio::test]
    async fn uncreated_store_is_first_login_missing_without_materializing_files() {
        let (temp, path) = temp_store();
        let (manager, web_provider) = manager_with_provider(
            PollResult::Waiting,
            Arc::new(FakeCredentialProvider::default()),
        );

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;

        let snapshot = manager.readiness("default", &Credentials::default());
        assert_eq!(CredentialReadiness::Missing, snapshot.web);
        assert_eq!(CredentialReadiness::Missing, snapshot.access_key);
        assert!(!path.exists());
        assert_eq!(
            0,
            fs::read_dir(temp.path())
                .expect("credential directory")
                .count()
        );
        assert_eq!(
            Code::FailedPrecondition,
            manager
                .start_access_key("default".to_owned(), Some(path), "https://media.example",)
                .await
                .expect_err("generic handoff requires a verified Web account")
                .code()
        );
        assert_eq!(0, web_provider.creates.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn healthy_web_credentials_are_reused_without_qr_and_keep_refresh_material() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, Some(REFRESH_TOKEN_A));
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        let (manager, web_provider) =
            manager_with_provider(PollResult::Waiting, credential_provider.clone());

        let session = manager
            .start("default".to_owned(), Some(path.clone()))
            .await
            .expect("healthy saved credentials are reused");

        assert_eq!(BilibiliLoginSessionState::Ready, session.state());
        assert!(session.verification_uri.is_empty());
        assert_eq!(0, web_provider.creates.load(Ordering::SeqCst));
        assert_eq!(1, credential_provider.refresh_calls.load(Ordering::SeqCst));
        let loaded = load_profile_bundle(&path, "default").expect("credentials remain readable");
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_A),
            loaded
                .secrets
                .cookie()
                .and_then(|secret| secret.refresh_token.as_deref())
        );
    }

    #[tokio::test]
    async fn missing_refresh_material_skips_provider_refresh() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        let manager = manager_with_provider(PollResult::Waiting, credential_provider.clone()).0;

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;

        assert_eq!(0, credential_provider.refresh_calls.load(Ordering::SeqCst));
        let loaded = load_profile_bundle(&path, "default").expect("credentials remain readable");
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
        assert_eq!(
            CredentialReadiness::Ready,
            manager.readiness("default", &loaded.credentials).web
        );
    }

    #[tokio::test]
    async fn refresh_replaces_only_verified_same_account_cookie_and_refresh_token() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, Some(REFRESH_TOKEN_A));
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.set_refresh(FakeRefreshOutcome::Refreshed {
            cookie: WEB_COOKIE_B,
            refresh_token: REFRESH_TOKEN_B,
        });
        let manager = manager_with_provider(PollResult::Waiting, credential_provider).0;

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;

        let loaded = load_profile_bundle(&path, "default").expect("refreshed profile");
        assert_eq!(Some(WEB_COOKIE_B), loaded.credentials.cookie.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_B),
            loaded
                .secrets
                .cookie()
                .and_then(|secret| secret.refresh_token.as_deref())
        );
        assert!(matches!(
            bilibili_credential_bindings::lookup(&path, "default", WEB_COOKIE_B)
                .expect("new binding matches refreshed cookie"),
            BindingLookup::Matches(binding) if binding.account_id() == 31_001
        ));
    }

    #[tokio::test]
    async fn refresh_rejection_requests_login_without_discarding_saved_material() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, Some(REFRESH_TOKEN_A));
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.set_refresh(FakeRefreshOutcome::Rejected);
        let manager = manager_with_provider(PollResult::Waiting, credential_provider).0;

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;

        let loaded = load_profile_bundle(&path, "default").expect("original profile retained");
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_A),
            loaded
                .secrets
                .cookie()
                .and_then(|secret| secret.refresh_token.as_deref())
        );
        assert_eq!(
            CredentialReadiness::LoginRequired,
            manager.readiness("default", &loaded.credentials).web
        );
    }

    #[tokio::test]
    async fn expired_cookie_refresh_requires_saved_binding_and_verifies_new_identity() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, Some(REFRESH_TOKEN_A));
        bilibili_credential_bindings::replace(&path, "default", WEB_COOKIE_A, 31_001, None)
            .expect("seed private binding for expired cookie");
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.set_identity(
            CredentialKind::Cookie,
            WEB_COOKIE_A,
            FakeIdentityOutcome::Rejected,
        );
        credential_provider.set_identity(
            CredentialKind::Cookie,
            WEB_COOKIE_B,
            FakeIdentityOutcome::Account(31_001),
        );
        credential_provider.set_refresh(FakeRefreshOutcome::Refreshed {
            cookie: WEB_COOKIE_B,
            refresh_token: REFRESH_TOKEN_B,
        });
        let manager = manager_with_provider(PollResult::Waiting, credential_provider).0;

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;

        let loaded = load_profile_bundle(&path, "default").expect("renewed profile");
        assert_eq!(Some(WEB_COOKIE_B), loaded.credentials.cookie.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_B),
            loaded
                .secrets
                .cookie()
                .and_then(|secret| secret.refresh_token.as_deref())
        );
        assert_eq!(
            CredentialReadiness::Ready,
            manager.readiness("default", &loaded.credentials).web
        );
    }

    #[tokio::test]
    async fn cancelled_maintenance_releases_profile_reservation() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        let started = credential_provider.pause_next_identity();
        let manager = manager_with_provider(PollResult::Waiting, credential_provider).0;
        let first_manager = manager.clone();
        let first_path = path.clone();
        let first = tokio::spawn(async move {
            first_manager
                .maintain_profile("default".to_owned(), first_path)
                .await;
        });
        started.notified().await;
        assert_eq!(
            Code::FailedPrecondition,
            manager
                .start("default".to_owned(), Some(path.clone()))
                .await
                .expect_err("maintenance and login share one profile reservation")
                .code()
        );
        first.abort();
        assert!(first.await.is_err());

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;
        let loaded = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert_eq!(
            CredentialReadiness::Ready,
            manager.readiness("default", &loaded.credentials).web
        );
    }

    #[tokio::test]
    async fn credential_provider_timeout_marks_unavailable_and_releases_reservation() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.pause_next_identity();
        let mut manager = manager_with_provider(PollResult::Waiting, credential_provider).0;
        manager.credential_timeout = Duration::from_millis(10);

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;
        let loaded = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert_eq!(
            CredentialReadiness::Unavailable,
            manager.readiness("default", &loaded.credentials).web
        );
        assert!(
            !manager
                .state
                .lock()
                .expect("state lock")
                .active_profiles
                .contains_key("default")
        );

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;
        assert_eq!(
            CredentialReadiness::Ready,
            manager.readiness("default", &loaded.credentials).web
        );
    }

    #[tokio::test]
    async fn provider_unavailability_is_not_login_required_and_keeps_saved_credentials() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, Some(REFRESH_TOKEN_A));
        let refresh_provider = Arc::new(FakeCredentialProvider::default());
        refresh_provider.set_refresh(FakeRefreshOutcome::Unavailable);
        let manager = manager_with_provider(PollResult::Waiting, refresh_provider).0;
        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;
        let loaded = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert_eq!(
            CredentialReadiness::Unavailable,
            manager.readiness("default", &loaded.credentials).web
        );
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_A),
            loaded
                .secrets
                .cookie()
                .and_then(|secret| secret.refresh_token.as_deref())
        );

        let identity_provider = Arc::new(FakeCredentialProvider::default());
        identity_provider.set_identity(
            CredentialKind::Cookie,
            WEB_COOKIE_A,
            FakeIdentityOutcome::Unavailable,
        );
        let manager = manager_with_provider(PollResult::Waiting, identity_provider).0;
        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;
        assert_eq!(
            CredentialReadiness::Unavailable,
            manager.readiness("default", &loaded.credentials).web
        );
    }

    #[tokio::test]
    async fn legacy_rejected_cookie_without_private_binding_fails_closed() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, Some(REFRESH_TOKEN_A));
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.set_identity(
            CredentialKind::Cookie,
            WEB_COOKIE_A,
            FakeIdentityOutcome::Rejected,
        );
        let (manager, web_provider) =
            manager_with_provider(PollResult::Waiting, credential_provider);

        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;

        let loaded =
            load_profile_bundle(&path, "default").expect("legacy profile remains readable");
        assert_eq!(
            CredentialReadiness::LoginRequired,
            manager.readiness("default", &loaded.credentials).web
        );
        assert!(matches!(
            bilibili_credential_bindings::lookup_profile(&path, "default"),
            Ok(None)
        ));
        assert_eq!(0, web_provider.creates.load(Ordering::SeqCst));
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
    }

    #[tokio::test]
    async fn verified_private_binding_survives_manager_restart_for_same_account_reauth() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, Some(REFRESH_TOKEN_A));
        let first_manager = manager(PollResult::Waiting);
        first_manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;
        assert!(matches!(
            bilibili_credential_bindings::lookup(&path, "default", WEB_COOKIE_A)
                .expect("private binding loads"),
            BindingLookup::Matches(binding) if binding.account_id() == 31_001
        ));

        let restarted_credentials = Arc::new(FakeCredentialProvider::default());
        restarted_credentials.set_identity(
            CredentialKind::Cookie,
            WEB_COOKIE_A,
            FakeIdentityOutcome::Rejected,
        );
        let (restarted, web_provider) =
            manager_with_provider(PollResult::Waiting, restarted_credentials);
        let session = restarted
            .start("default".to_owned(), Some(path))
            .await
            .expect("stored identity binding authorizes same-account reauthorization");

        assert_eq!(BilibiliLoginSessionState::Pending, session.state());
        assert_eq!(1, web_provider.creates.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn web_reauthorization_rejects_a_different_official_account() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, Some(REFRESH_TOKEN_A));
        bilibili_credential_bindings::replace(&path, "default", WEB_COOKIE_A, 31_001, None)
            .expect("seed verified private account binding");
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.set_identity(
            CredentialKind::Cookie,
            WEB_COOKIE_A,
            FakeIdentityOutcome::Rejected,
        );
        credential_provider.set_identity(
            CredentialKind::Cookie,
            WEB_COOKIE_B,
            FakeIdentityOutcome::Account(31_002),
        );
        let (manager, _) = manager_with_provider(
            PollResult::Succeeded {
                credentials: Credentials::default().with_cookie(WEB_COOKIE_B),
                refresh_token: Some(REFRESH_TOKEN_B.to_owned()),
            },
            credential_provider,
        );

        let session = manager
            .start("default".to_owned(), Some(path.clone()))
            .await
            .expect("re-authorization starts from existing verified account");
        let terminal = wait_terminal(&manager, &session.id).await;

        assert_eq!(BilibiliLoginSessionState::Error, terminal.state());
        let loaded = load_profile_bundle(&path, "default").expect("saved profile remains readable");
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_A),
            loaded
                .secrets
                .cookie()
                .and_then(|secret| secret.refresh_token.as_deref())
        );
        assert!(matches!(
            bilibili_credential_bindings::lookup(&path, "default", WEB_COOKIE_A)
                .expect("old binding remains intact"),
            BindingLookup::Matches(binding) if binding.account_id() == 31_001
        ));
    }

    fn access_key_message() -> String {
        format!(
            "balh-login-credentials: {{\"access_key\":\"{ACCESS_KEY_A}\",\"refresh_token\":\"{REFRESH_TOKEN_A}\"}}"
        )
    }

    fn qr_session_record(
        public: BilibiliLoginSession,
        ticket_key: Option<String>,
        deadline: Instant,
        finishing: bool,
        completed_at: Option<Instant>,
    ) -> SessionRecord {
        SessionRecord {
            public,
            credential_path: None,
            method: LoginMethod::WebQr,
            ticket_key,
            access_ticket: None,
            capability: None,
            server_origin: None,
            expected_account_id: None,
            expected_web_binding: None,
            baseline: CredentialBaseline::default(),
            deadline,
            finishing,
            completed_at,
        }
    }

    async fn start_access_key_session(
        manager: &BilibiliLoginManager,
        path: &Path,
    ) -> BilibiliLoginSession {
        manager
            .start_access_key(
                "default".to_owned(),
                Some(path.to_path_buf()),
                "https://media.example/cache",
            )
            .await
            .expect("access-key browser session starts")
    }

    #[tokio::test]
    async fn browser_callback_requires_origin_capability_and_profile_reservation_once() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let manager = manager(PollResult::Waiting);
        let session = start_access_key_session(&manager, &path).await;
        let capability = session
            .verification_uri
            .split_once('#')
            .expect("fragment capability is issued")
            .1
            .to_owned();
        assert!(
            session
                .verification_uri
                .starts_with("https://media.example/cache/login/bilibili/")
        );
        assert_eq!(64, capability.len());
        let page = manager
            .browser_page(&session.id, "https://media.example")
            .expect("page uses the issued origin");
        assert!(!page.contains(&capability));
        assert!(page.contains("<script>\n"));
        assert!(page.contains("event.source !== providerWindow"));
        assert!(page.contains("message_origin: event.origin"));
        assert!(page.contains("message: event.data"));

        assert_eq!(
            Code::PermissionDenied,
            manager
                .complete_browser_login(
                    &session.id,
                    "https://evil.example",
                    &capability,
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
                .expect_err("issued capability does not override wrong request origin")
                .code()
        );
        assert_eq!(
            Code::PermissionDenied,
            manager
                .complete_browser_login(
                    &session.id,
                    "https://media.example",
                    "wrong-capability",
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
                .expect_err("exact Origin alone is not authority")
                .code()
        );
        {
            let mut state = manager.state.lock().expect("state lock");
            state
                .active_profiles
                .insert("default".to_owned(), "another-session".to_owned());
        }
        assert_eq!(
            Code::PermissionDenied,
            manager
                .complete_browser_login(
                    &session.id,
                    "https://media.example",
                    &capability,
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
                .expect_err("completion must own the profile reservation")
                .code()
        );
        {
            let mut state = manager.state.lock().expect("state lock");
            state
                .active_profiles
                .insert("default".to_owned(), session.id.clone());
        }
        manager
            .complete_browser_login(
                &session.id,
                "https://media.example",
                &capability,
                "https://www.biliplus.com",
                &access_key_message(),
            )
            .await
            .expect("first authorized completion persists the generic key");

        let loaded = load_profile_bundle(&path, "default").expect("access-key profile saved");
        assert_eq!(Some(ACCESS_KEY_A), loaded.credentials.access_key.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_A),
            loaded
                .secrets
                .access_key_provider(AccessKeyProvider::BalhBiliplus)
                .and_then(|secret| secret.refresh_token.as_deref())
        );
        assert_eq!(
            CredentialReadiness::Ready,
            manager.readiness("default", &loaded.credentials).access_key
        );
        assert_eq!(
            Code::PermissionDenied,
            manager
                .complete_browser_login(
                    &session.id,
                    "https://media.example",
                    &capability,
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
                .expect_err("terminal callback cannot replay")
                .code()
        );
    }

    #[tokio::test]
    async fn expired_browser_capability_cannot_complete_or_persist() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let manager = manager(PollResult::Waiting);
        let session = start_access_key_session(&manager, &path).await;
        let capability = session
            .verification_uri
            .split_once('#')
            .unwrap()
            .1
            .to_owned();
        {
            let mut state = manager.state.lock().expect("state lock");
            state.sessions.get_mut(&session.id).unwrap().deadline =
                Instant::now() - Duration::from_secs(1);
        }

        assert_eq!(
            Code::PermissionDenied,
            manager
                .complete_browser_login(
                    &session.id,
                    "https://media.example",
                    &capability,
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
                .expect_err("expired session denies completion")
                .code()
        );
        assert_eq!(
            BilibiliLoginSessionState::Expired,
            manager
                .get(&session.id)
                .expect("expired session remains queryable")
                .state()
        );
        let loaded = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert!(loaded.credentials.access_key.is_none());
    }

    #[tokio::test]
    async fn cancelled_browser_completion_can_retry_and_release_profile_reservation() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        let manager =
            manager_with_provider(PollResult::Waiting, Arc::clone(&credential_provider)).0;
        let session = start_access_key_session(&manager, &path).await;
        let capability = session
            .verification_uri
            .split_once('#')
            .unwrap()
            .1
            .to_owned();
        let probe_started = credential_provider.pause_next_identity();
        let callback_manager = manager.clone();
        let callback_session_id = session.id.clone();
        let callback_capability = capability.clone();
        let callback = tokio::spawn(async move {
            callback_manager
                .complete_browser_login(
                    &callback_session_id,
                    "https://media.example",
                    &callback_capability,
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
        });

        probe_started.notified().await;
        assert!(
            manager
                .state
                .lock()
                .expect("state lock")
                .sessions
                .get(&session.id)
                .expect("session record")
                .finishing
        );
        callback.abort();
        assert!(
            callback
                .await
                .expect_err("callback task was cancelled")
                .is_cancelled()
        );

        {
            let state = manager.state.lock().expect("state lock");
            let record = state
                .sessions
                .get(&session.id)
                .expect("session remains retryable");
            assert_eq!(BilibiliLoginSessionState::Pending, record.public.state());
            assert!(!record.finishing);
            assert_eq!(
                Some(session.id.as_str()),
                state.active_profiles.get("default").map(String::as_str)
            );
        }
        let loaded = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert!(loaded.credentials.access_key.is_none());

        manager
            .complete_browser_login(
                &session.id,
                "https://media.example",
                &capability,
                "https://www.biliplus.com",
                &access_key_message(),
            )
            .await
            .expect("the same pending session can retry");
        assert_eq!(
            BilibiliLoginSessionState::Ready,
            manager.get(&session.id).expect("completed session").state()
        );
        assert!(
            !manager
                .state
                .lock()
                .expect("state lock")
                .active_profiles
                .contains_key("default")
        );
        let loaded = load_profile_bundle(&path, "default").expect("committed profile");
        assert_eq!(Some(ACCESS_KEY_A), loaded.credentials.access_key.as_deref());
        assert_eq!(
            Some(REFRESH_TOKEN_A),
            loaded
                .secrets
                .access_key_provider(AccessKeyProvider::BalhBiliplus)
                .and_then(|secret| secret.refresh_token.as_deref())
        );
    }

    #[tokio::test]
    async fn browser_completion_does_not_persist_after_probe_crosses_deadline() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        let manager =
            manager_with_provider(PollResult::Waiting, Arc::clone(&credential_provider)).0;
        let session = start_access_key_session(&manager, &path).await;
        let capability = session
            .verification_uri
            .split_once('#')
            .unwrap()
            .1
            .to_owned();
        let (probe_started, resume_probe) = credential_provider.pause_next_identity_with_release();
        let callback_manager = manager.clone();
        let callback_session_id = session.id.clone();
        let callback_capability = capability.clone();
        let callback = tokio::spawn(async move {
            callback_manager
                .complete_browser_login(
                    &callback_session_id,
                    "https://media.example",
                    &callback_capability,
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
        });

        probe_started.notified().await;
        {
            let mut state = manager.state.lock().expect("state lock");
            state
                .sessions
                .get_mut(&session.id)
                .expect("session record")
                .deadline = Instant::now() - Duration::from_secs(1);
        }
        assert_eq!(
            BilibiliLoginSessionState::Pending,
            manager.get(&session.id).expect("finishing session").state()
        );
        resume_probe.notify_one();

        assert_eq!(
            Code::Aborted,
            callback
                .await
                .expect("callback task completed")
                .expect_err("expired completion must not persist")
                .code()
        );
        assert_eq!(
            BilibiliLoginSessionState::Expired,
            manager.get(&session.id).expect("expired session").state()
        );
        assert!(
            !manager
                .state
                .lock()
                .expect("state lock")
                .active_profiles
                .contains_key("default")
        );
        let loaded = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
        assert!(loaded.credentials.access_key.is_none());
        assert!(
            loaded
                .secrets
                .access_key_provider(AccessKeyProvider::BalhBiliplus)
                .is_none()
        );
    }

    #[tokio::test]
    async fn browser_callback_rejects_account_mismatch_without_committing() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let credential_provider = Arc::new(FakeCredentialProvider::default());
        credential_provider.set_identity(
            CredentialKind::AccessKey,
            ACCESS_KEY_A,
            FakeIdentityOutcome::Account(31_002),
        );
        let manager = manager_with_provider(PollResult::Waiting, credential_provider).0;
        let session = start_access_key_session(&manager, &path).await;
        let capability = session
            .verification_uri
            .split_once('#')
            .unwrap()
            .1
            .to_owned();

        assert_eq!(
            Code::FailedPrecondition,
            manager
                .complete_browser_login(
                    &session.id,
                    "https://media.example",
                    &capability,
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
                .expect_err("different official account cannot replace credentials")
                .code()
        );
        let loaded = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert!(loaded.credentials.access_key.is_none());
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
    }

    #[tokio::test]
    async fn browser_callback_rejects_untrusted_provider_message_origin() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let manager = manager(PollResult::Waiting);
        let session = start_access_key_session(&manager, &path).await;
        let capability = session
            .verification_uri
            .split_once('#')
            .unwrap()
            .1
            .to_owned();

        assert_eq!(
            Code::PermissionDenied,
            manager
                .complete_browser_login(
                    &session.id,
                    "https://media.example",
                    &capability,
                    "https://evil.example",
                    &access_key_message(),
                )
                .await
                .expect_err("provider event origin must match the issued ticket")
                .code()
        );
        let loaded = load_profile_bundle(&path, "default").expect("profile remains readable");
        assert!(loaded.credentials.access_key.is_none());
        assert_eq!(
            BilibiliLoginSessionState::Error,
            manager
                .get(&session.id)
                .expect("failed completion remains queryable")
                .state()
        );
    }

    #[tokio::test]
    async fn browser_callback_content_compare_and_replace_rejects_stale_baseline() {
        let (_temp, path) = temp_store();
        seed_web_profile(&path, WEB_COOKIE_A, None);
        let manager = manager(PollResult::Waiting);
        let session = start_access_key_session(&manager, &path).await;
        let capability = session
            .verification_uri
            .split_once('#')
            .unwrap()
            .1
            .to_owned();
        CredentialStore::new(path.clone())
            .update_profiles(|profiles| {
                let mut current = profiles.profile("default")?;
                current.access_key = Some(ACCESS_KEY_B.to_owned());
                profiles.set_profile("default", current)
            })
            .expect("simulate concurrent profile update");

        assert_eq!(
            Code::Aborted,
            manager
                .complete_browser_login(
                    &session.id,
                    "https://media.example",
                    &capability,
                    "https://www.biliplus.com",
                    &access_key_message(),
                )
                .await
                .expect_err("stale callback cannot replace a concurrent edit")
                .code()
        );
        let loaded = load_profile_bundle(&path, "default").expect("concurrent profile remains");
        assert_eq!(Some(ACCESS_KEY_B), loaded.credentials.access_key.as_deref());
        assert!(
            loaded
                .secrets
                .access_key_provider(AccessKeyProvider::BalhBiliplus)
                .is_none()
        );
    }

    #[tokio::test]
    async fn failed_private_binding_publication_never_marks_saved_web_cookie_ready() {
        let (_temp, path) = temp_store();
        let sidecar = PathBuf::from(format!("{}.bilibili-bindings.json", path.display()));
        fs::create_dir(&sidecar).expect("block sidecar publication");
        assert!(!persist_web_login(
            &path,
            "default",
            &CredentialBaseline::default(),
            None,
            WEB_COOKIE_A,
            Some(REFRESH_TOKEN_A),
            31_001,
        ));

        let loaded = load_profile_bundle(&path, "default").expect("profile transaction committed");
        assert_eq!(Some(WEB_COOKIE_A), loaded.credentials.cookie.as_deref());
        let manager = manager(PollResult::Waiting);
        manager
            .maintain_profile("default".to_owned(), path.clone())
            .await;
        assert_eq!(
            CredentialReadiness::Unavailable,
            manager.readiness("default", &loaded.credentials).web
        );
        assert_eq!(
            Code::Unavailable,
            manager
                .start_access_key("default".to_owned(), Some(path), "https://media.example",)
                .await
                .expect_err("unreadable private binding blocks generic handoff")
                .code()
        );
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
    async fn reuses_existing_web_cookie_without_replacing_it() {
        let (_temp, path) = temp_store();
        CredentialStore::new(path.clone())
            .save(&Credentials::default().with_cookie("existing-cookie"))
            .expect("seed cookie");
        let (manager, web_provider) = manager_with_provider(
            PollResult::Waiting,
            Arc::new(FakeCredentialProvider::default()),
        );
        let session = manager
            .start("default".to_owned(), Some(path.clone()))
            .await
            .expect("healthy cookie is reused");
        assert_eq!(BilibiliLoginSessionState::Ready, session.state());
        assert!(session.verification_uri.is_empty());
        assert_eq!(0, web_provider.creates.load(Ordering::SeqCst));
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
                qr_session_record(
                    BilibiliLoginSession {
                        id: session_id.to_owned(),
                        profile_id: profile_id.to_owned(),
                        method: 1,
                        state: BilibiliLoginSessionState::Pending.into(),
                        message: String::new(),
                        verification_uri: String::new(),
                        created_at: None,
                        expires_at: None,
                    },
                    Some("private-ticket".to_owned()),
                    Instant::now() - Duration::from_secs(1),
                    true,
                    None,
                ),
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
            qr_session_record(
                session,
                Some(String::new()),
                Instant::now(),
                false,
                Some(Instant::now() - TERMINAL_SESSION_TTL - Duration::from_secs(1)),
            ),
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
                qr_session_record(
                    BilibiliLoginSession {
                        id: "completed".to_owned(),
                        profile_id: "first".to_owned(),
                        method: 1,
                        state: BilibiliLoginSessionState::Ready.into(),
                        message: "done".to_owned(),
                        verification_uri: String::new(),
                        created_at: Some(current_timestamp()),
                        expires_at: Some(timestamp_after(SESSION_TTL)),
                    },
                    Some("private-ticket".to_owned()),
                    Instant::now(),
                    false,
                    Some(Instant::now()),
                ),
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
                    qr_session_record(
                        BilibiliLoginSession {
                            id,
                            profile_id: profile,
                            method: 1,
                            state: BilibiliLoginSessionState::Pending.into(),
                            message: String::new(),
                            verification_uri: String::new(),
                            created_at: None,
                            expires_at: None,
                        },
                        Some("private-ticket".to_owned()),
                        Instant::now() + SESSION_TTL,
                        false,
                        None,
                    ),
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
            qr_session_record(
                BilibiliLoginSession {
                    id: "id".to_owned(),
                    profile_id: "default".to_owned(),
                    method: 1,
                    state: BilibiliLoginSessionState::Pending.into(),
                    message: String::new(),
                    verification_uri: String::new(),
                    created_at: None,
                    expires_at: None,
                },
                Some("secret-ticket".to_owned()),
                Instant::now(),
                false,
                None,
            ),
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
                qr_session_record(
                    session,
                    Some("secret-ticket".to_owned()),
                    Instant::now() + SESSION_TTL,
                    false,
                    None,
                ),
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
