use std::{future::Future, pin::Pin, time::Duration};

use bbdown_core::{
    BiliClient, ClientConfig, CredentialAccountIdentity, CredentialHealthStatus, CredentialKind,
    Credentials, WebCookieRefreshRequest,
};

pub(crate) const CREDENTIAL_PROVIDER_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialReadiness {
    Unknown,
    Missing,
    Checking,
    Ready,
    LoginRequired,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CredentialReadinessSnapshot {
    pub(crate) web: CredentialReadiness,
    pub(crate) access_key: CredentialReadiness,
}

pub(crate) type CredentialFuture<T> =
    Pin<Box<dyn Future<Output = Result<T, CredentialProviderError>> + Send>>;

pub(crate) struct WebCookieRefreshResult {
    pub(crate) cookie: String,
    pub(crate) refresh_token: String,
    pub(crate) refreshed: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct VerifiedIdentity {
    pub(crate) kind: CredentialKind,
    pub(crate) account_id: u64,
}

pub(crate) enum CredentialProviderError {
    Rejected,
    Unavailable,
}

pub(crate) trait CredentialProvider: Send + Sync {
    fn account_identity(
        &self,
        credentials: Credentials,
        kind: CredentialKind,
    ) -> CredentialFuture<VerifiedIdentity>;

    fn refresh_web_cookie(
        &self,
        cookie: String,
        refresh_token: String,
    ) -> CredentialFuture<WebCookieRefreshResult>;
}

pub(crate) struct BbdownCredentialProvider;

impl CredentialProvider for BbdownCredentialProvider {
    fn account_identity(
        &self,
        credentials: Credentials,
        kind: CredentialKind,
    ) -> CredentialFuture<VerifiedIdentity> {
        Box::pin(async move {
            let client = client_with_credentials(credentials);
            let identity = match client.credential_account_identity(kind).await {
                Ok(identity) => identity,
                Err(_) => return Err(classify_credential_failure(&client, kind).await),
            };
            Ok(verified_identity(identity))
        })
    }

    fn refresh_web_cookie(
        &self,
        cookie: String,
        refresh_token: String,
    ) -> CredentialFuture<WebCookieRefreshResult> {
        Box::pin(async move {
            let client =
                client_with_credentials(Credentials::default().with_cookie(cookie.clone()));
            let request = WebCookieRefreshRequest::new(cookie, refresh_token)
                .map_err(|_| CredentialProviderError::Rejected)?;
            match client.refresh_web_cookie(&request).await {
                Ok(credentials) => Ok(WebCookieRefreshResult {
                    cookie: credentials.cookie,
                    refresh_token: credentials.refresh_token,
                    refreshed: credentials.refreshed,
                }),
                Err(_) => Err(classify_credential_failure(&client, CredentialKind::Cookie).await),
            }
        })
    }
}

fn client_with_credentials(credentials: Credentials) -> BiliClient {
    BiliClient::new(
        ClientConfig::default()
            .with_credentials(credentials)
            .with_request_timeout(CREDENTIAL_PROVIDER_TIMEOUT),
    )
}

fn verified_identity(identity: CredentialAccountIdentity) -> VerifiedIdentity {
    VerifiedIdentity {
        kind: identity.kind,
        account_id: identity.account_id,
    }
}

async fn classify_credential_failure(
    client: &BiliClient,
    kind: CredentialKind,
) -> CredentialProviderError {
    let health = client.check_credential_health().await;
    match health.probes.iter().find(|probe| probe.kind == kind) {
        Some(probe) if probe.status == CredentialHealthStatus::Rejected => {
            CredentialProviderError::Rejected
        }
        _ => CredentialProviderError::Unavailable,
    }
}
