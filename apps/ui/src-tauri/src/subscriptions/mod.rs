//! Shared native command and streaming event contract for subscription providers.
//!
//! Tauri handlers planned against these types:
//! - `subscription_get_status(provider)`
//! - `subscription_start_login(provider)` / `subscription_cancel_login(provider, login_id)`
//! - `subscription_sign_out(provider)`
//! - `subscription_list_models(provider, account_generation)`
//! - `subscription_start_request(request, channel)` / `subscription_cancel_request(request_id)`
//!
//! The invoking `WebviewWindow` is injected by Tauri and scopes login challenges, requests,
//! channels, and continuation IDs. Native code derives fixed provider hosts and authorization
//! headers; no command accepts a URL, token, or header. The request channel sends exactly one
//! response event before zero or more ordered chunks and exactly one terminal event. Response
//! headers are allowlisted (currently content-type only); error messages never include provider
//! bodies, URLs, headers, or credentials.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::{Arc, Mutex as StdMutex},
};

pub mod auth;
pub mod transport;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionProvider {
    CodexSubscription,
    GrokSubscription,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionAccountState {
    SignedOut,
    Pending,
    SignedIn,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoginKind {
    BrowserPkce,
    DeviceCode,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionAccountStatus {
    pub provider: SubscriptionProvider,
    pub state: SubscriptionAccountState,
    /// Opaque account identity only; never an access or refresh token.
    pub account_id: Option<String>,
    /// Incremented on sign-in, sign-out, or account replacement to invalidate stale work.
    pub generation: u64,
    pub message: Option<String>,
}

/// Rust-only authenticated material. Deliberately has no `Serialize`, `Debug`, or `Clone` impl.
pub struct AuthorizedSession {
    pub(crate) access_token: String,
    pub(crate) account_id: Option<String>,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError {
    SignedOut,
    StaleGeneration,
    RefreshRejected,
    StorageUnavailable,
}

/// Shared auth/transport boundary. Implementations persist secrets in native secure storage,
/// serialize refresh per provider, and discard any refresh result whose generation went stale.
pub trait SubscriptionSessions: Send + Sync {
    fn authorized(
        &self,
        provider: SubscriptionProvider,
        expected_generation: u64,
    ) -> impl Future<Output = Result<AuthorizedSession, SessionError>> + Send;

    /// Single-flight refresh after a 401. `used_access_token` prevents replaying a rotated token;
    /// concurrent callers share the same refresh, and generation is rechecked before persistence.
    fn refresh_after_unauthorized(
        &self,
        provider: SubscriptionProvider,
        expected_generation: u64,
        used_access_token: &str,
    ) -> impl Future<Output = Result<AuthorizedSession, SessionError>> + Send;

    fn generation(&self, provider: SubscriptionProvider) -> impl Future<Output = u64> + Send;

    /// Clears the provider session and increments generation before returning.
    fn sign_out(
        &self,
        provider: SubscriptionProvider,
    ) -> impl Future<Output = Result<u64, SessionError>> + Send;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionLoginChallenge {
    pub kind: LoginKind,
    pub login_id: String,
    pub verification_url: String,
    /// Empty for browser PKCE; never contains the device authorization token.
    pub user_code: String,
    /// Unix epoch milliseconds.
    pub expires_at: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilitySupport {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiBackend {
    ChatCompletions,
    Responses,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionModelInfo {
    pub id: String,
    pub name: String,
    pub api_backend: ApiBackend,
    pub images: CapabilitySupport,
    pub reasoning: CapabilitySupport,
    pub tools: CapabilitySupport,
    pub recommended: bool,
    pub context_window: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionRequest {
    pub request_id: String,
    pub provider: SubscriptionProvider,
    pub account_generation: u64,
    pub model_id: String,
    /// Untrusted protocol input. Native validates and overwrites routing and safety fields.
    pub body: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum SubscriptionRequestEvent {
    Response {
        request_id: String,
        sequence: u64,
        status: u16,
        headers: BTreeMap<String, String>,
    },
    Chunk {
        request_id: String,
        sequence: u64,
        bytes: Vec<u8>,
    },
    Complete {
        request_id: String,
        sequence: u64,
    },
    Error {
        request_id: String,
        sequence: u64,
        message: String,
    },
}

#[derive(Clone)]
pub struct SubscriptionRuntime {
    pub auth: auth::NativeSubscriptionAuth,
    pub transport: Arc<transport::SubscriptionTransport<auth::NativeSubscriptionAuth>>,
    login_owners: LoginOwners,
}

type LoginOwners = Arc<StdMutex<HashMap<String, (String, SubscriptionProvider, u64)>>>;

impl SubscriptionRuntime {
    pub fn new() -> Result<Self, String> {
        let auth = auth::NativeSubscriptionAuth::new()
            .map_err(|_| "Could not initialize secure subscription authentication.".to_owned())?;
        let transport = Arc::new(transport::SubscriptionTransport::new(Arc::new(
            auth.clone(),
        ))?);
        Ok(Self {
            auth,
            transport,
            login_owners: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    pub async fn start_login(
        &self,
        provider: SubscriptionProvider,
        window_label: &str,
    ) -> Result<SubscriptionLoginChallenge, String> {
        let challenge = self
            .auth
            .start_login(provider)
            .await
            .map_err(sanitize_auth_error)?;
        if let Err(error) = self.record_login_owner(
            challenge.login_id.clone(),
            provider,
            window_label,
            challenge.expires_at,
        ) {
            let _ = self.auth.cancel_login(provider, &challenge.login_id).await;
            return Err(error);
        }
        Ok(challenge)
    }

    pub async fn sign_out(&self, provider: SubscriptionProvider) -> Result<u64, String> {
        let generation = self
            .auth
            .sign_out(provider)
            .await
            .map_err(transport::sanitize_session_error)?;
        self.transport.cancel_provider(provider).await;
        self.transport.invalidate_catalog(provider).await;
        if let Ok(mut owners) = self.login_owners.lock() {
            owners.retain(|_, (_, owner_provider, _)| *owner_provider != provider);
        }
        Ok(generation)
    }

    pub fn record_login_owner(
        &self,
        login_id: String,
        provider: SubscriptionProvider,
        window_label: &str,
        expires_at: u64,
    ) -> Result<(), String> {
        let mut owners = self
            .login_owners
            .lock()
            .map_err(|_| "Subscription login state is unavailable.".to_owned())?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        owners.retain(|_, (_, _, expires_at)| *expires_at > now_ms);
        owners.insert(login_id, (window_label.to_owned(), provider, expires_at));
        Ok(())
    }

    pub async fn cancel_login(
        &self,
        provider: SubscriptionProvider,
        login_id: &str,
        window_label: &str,
    ) -> Result<(), String> {
        let owns_challenge = {
            let mut owners = self
                .login_owners
                .lock()
                .map_err(|_| "Subscription login state is unavailable.".to_owned())?;
            if owners
                .get(login_id)
                .is_some_and(|(owner, owner_provider, expires_at)| {
                    owner == window_label && *owner_provider == provider && *expires_at > now_ms()
                })
            {
                owners.remove(login_id);
                true
            } else {
                false
            }
        };
        if owns_challenge {
            self.auth
                .cancel_login(provider, login_id)
                .await
                .map_err(sanitize_auth_error)
        } else {
            Err("That subscription login belongs to another window or has expired.".to_owned())
        }
    }

    pub async fn cancel_window_logins(&self, window_label: &str) {
        let ids = self
            .login_owners
            .lock()
            .map(|mut owners| {
                let ids = owners
                    .iter()
                    .filter_map(|(login_id, (owner, provider, _))| {
                        (owner == window_label).then_some((login_id.clone(), *provider))
                    })
                    .collect::<Vec<_>>();
                for (login_id, _) in &ids {
                    owners.remove(login_id);
                }
                ids
            })
            .unwrap_or_default();
        for (login_id, provider) in ids {
            let _ = self.auth.cancel_login(provider, &login_id).await;
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn sanitize_auth_error(error: auth::AuthError) -> String {
    match error {
        auth::AuthError::Cancelled => "Subscription sign-in was cancelled.".to_owned(),
        auth::AuthError::Expired => {
            "The subscription sign-in request expired. Try again.".to_owned()
        }
        auth::AuthError::Denied => "The subscription provider declined sign-in.".to_owned(),
        auth::AuthError::Network => {
            "Could not connect to the subscription sign-in service.".to_owned()
        }
        auth::AuthError::InvalidResponse => {
            "The subscription sign-in response was invalid.".to_owned()
        }
        auth::AuthError::StorageUnavailable => {
            "The operating system could not access the subscription keychain.".to_owned()
        }
        auth::AuthError::AlreadyPending => {
            "A subscription sign-in is already in progress.".to_owned()
        }
    }
}
