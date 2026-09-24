//! Native subscription authentication.
//!
//! Refresh credentials are stored as one Keychain/credential-manager item per provider. Access
//! tokens remain in memory. No API returns a credential to the webview.

use super::{
    AuthorizedSession, LoginKind, SessionError, SubscriptionAccountState,
    SubscriptionAccountStatus, SubscriptionLoginChallenge, SubscriptionProvider,
    SubscriptionSessions,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use keyring::Entry;
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_ISSUER: &str = "https://auth.openai.com";
const CODEX_CALLBACK: &str = "http://localhost:1455/auth/callback";
const GROK_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const GROK_ISSUER: &str = "https://auth.x.ai";
const KEYRING_SERVICE: &str = "com.openscad-studio.subscriptions";
const MAX_LOGIN_LIFETIME: Duration = Duration::from_secs(5 * 60);

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Synchronous storage boundary so tests can use a synthetic in-memory store.
pub trait CredentialStore: Send + Sync + 'static {
    fn read(&self, provider: SubscriptionProvider) -> Result<Option<String>, ()>;
    fn write(&self, provider: SubscriptionProvider, value: &str) -> Result<(), ()>;
    fn delete(&self, provider: SubscriptionProvider) -> Result<(), ()>;
}

#[derive(Default)]
pub struct KeychainCredentialStore;

impl CredentialStore for KeychainCredentialStore {
    fn read(&self, provider: SubscriptionProvider) -> Result<Option<String>, ()> {
        let entry = credential_entry(provider)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(()),
        }
    }

    fn write(&self, provider: SubscriptionProvider, value: &str) -> Result<(), ()> {
        credential_entry(provider)?
            .set_password(value)
            .map_err(|_| ())
    }

    fn delete(&self, provider: SubscriptionProvider) -> Result<(), ()> {
        match credential_entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(()),
        }
    }
}

fn credential_entry(provider: SubscriptionProvider) -> Result<Entry, ()> {
    Entry::new(KEYRING_SERVICE, provider_key(provider)).map_err(|_| ())
}

fn provider_key(provider: SubscriptionProvider) -> &'static str {
    match provider {
        SubscriptionProvider::CodexSubscription => "codex-subscription",
        SubscriptionProvider::GrokSubscription => "grok-subscription",
    }
}

#[derive(Clone)]
pub struct NativeSubscriptionAuth {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    http: Client,
    store: Arc<dyn CredentialStore>,
    states: [Mutex<SessionState>; 2],
    refresh_locks: [Mutex<()>; 2],
    login_start_locks: [Mutex<()>; 2],
    logins: StdMutex<HashMap<String, PendingLogin>>,
    endpoints: EndpointSet,
}

#[derive(Default)]
struct SessionState {
    generation: u64,
    access_token: Option<String>,
    account_id: Option<String>,
    access_expires_at: Option<Instant>,
}

struct PendingLogin {
    provider: SubscriptionProvider,
    cancellation: CancellationToken,
}

#[derive(Clone)]
struct EndpointSet {
    codex_issuer: String,
    codex_callback: String,
    grok_issuer: String,
}

impl Default for EndpointSet {
    fn default() -> Self {
        Self {
            codex_issuer: CODEX_ISSUER.into(),
            codex_callback: CODEX_CALLBACK.into(),
            grok_issuer: GROK_ISSUER.into(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredCredential {
    refresh_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: Option<String>,
}

#[derive(Deserialize)]
struct GrokDeviceStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct CodexDeviceStart {
    device_auth_id: String,
    user_code: String,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    interval: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct CodexDevicePoll {
    authorization_code: Option<String>,
    code_verifier: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Cancelled,
    Expired,
    Denied,
    Network,
    InvalidResponse,
    StorageUnavailable,
    AlreadyPending,
}

impl NativeSubscriptionAuth {
    pub fn new() -> Result<Self, AuthError> {
        Self::with_store(Arc::new(KeychainCredentialStore))
    }

    pub fn with_store(store: Arc<dyn CredentialStore>) -> Result<Self, AuthError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("OpenSCAD-Studio/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| AuthError::Network)?;
        Ok(Self {
            inner: Arc::new(AuthInner {
                http,
                store,
                states: [
                    Mutex::new(SessionState::default()),
                    Mutex::new(SessionState::default()),
                ],
                refresh_locks: [Mutex::new(()), Mutex::new(())],
                login_start_locks: [Mutex::new(()), Mutex::new(())],
                logins: StdMutex::new(HashMap::new()),
                endpoints: EndpointSet::default(),
            }),
        })
    }

    #[cfg(test)]
    fn with_test_store(store: Arc<dyn CredentialStore>, issuer: String) -> Self {
        let mut auth = Self::with_store(store).expect("test HTTP client");
        Arc::get_mut(&mut auth.inner)
            .expect("unique test auth")
            .endpoints = EndpointSet {
            codex_issuer: issuer.clone(),
            codex_callback: format!("{issuer}/auth/callback"),
            grok_issuer: issuer,
        };
        auth
    }

    fn state(&self, provider: SubscriptionProvider) -> &Mutex<SessionState> {
        &self.inner.states[provider_index(provider)]
    }

    /// Starts the provider's browser PKCE flow or device-code fallback and returns only display
    /// information. The credential exchange continues in a native task.
    pub async fn start_login(
        &self,
        provider: SubscriptionProvider,
    ) -> Result<SubscriptionLoginChallenge, AuthError> {
        let _start = self.inner.login_start_locks[provider_index(provider)]
            .lock()
            .await;
        let starting_generation = self.state(provider).lock().await.generation;
        if self
            .inner
            .logins
            .lock()
            .map_err(|_| AuthError::InvalidResponse)?
            .values()
            .any(|pending| pending.provider == provider)
        {
            return Err(AuthError::AlreadyPending);
        }

        let (challenge, future) = match provider {
            SubscriptionProvider::GrokSubscription => self.start_grok_device_login().await?,
            SubscriptionProvider::CodexSubscription => {
                match self.start_codex_browser_login().await {
                    Ok(value) => value,
                    Err(_) => self.start_codex_device_login().await?,
                }
            }
        };
        let login_id = challenge.login_id.clone();
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let auth = self.clone();
        self.inner
            .logins
            .lock()
            .map_err(|_| AuthError::InvalidResponse)?
            .insert(
                login_id.clone(),
                PendingLogin {
                    provider,
                    cancellation: cancellation.clone(),
                },
            );
        let task = tokio::spawn(async move {
            let result = tokio::select! {
                _ = task_cancel.cancelled() => Err(AuthError::Cancelled),
                result = future => result,
            };
            if let Ok(tokens) = result {
                let _ = auth
                    .install_tokens(
                        provider,
                        starting_generation,
                        &login_id,
                        &task_cancel,
                        tokens,
                    )
                    .await;
            }
            if let Ok(mut logins) = auth.inner.logins.lock() {
                logins.remove(&login_id);
            }
        });
        drop(task);
        Ok(challenge)
    }

    pub async fn cancel_login(
        &self,
        provider: SubscriptionProvider,
        login_id: &str,
    ) -> Result<(), AuthError> {
        let _state = self.state(provider).lock().await;
        let mut logins = self
            .inner
            .logins
            .lock()
            .map_err(|_| AuthError::InvalidResponse)?;
        if logins
            .get(login_id)
            .is_some_and(|pending| pending.provider == provider)
        {
            if let Some(pending) = logins.remove(login_id) {
                pending.cancellation.cancel();
            }
        }
        Ok(())
    }

    pub async fn status(&self, provider: SubscriptionProvider) -> SubscriptionAccountStatus {
        let (generation, account_id, in_memory, stored) = loop {
            let before = self.state(provider).lock().await.generation;
            let stored = self.read_stored(provider).await.ok().flatten().is_some();
            let state = self.state(provider).lock().await;
            if state.generation == before {
                break (
                    state.generation,
                    state.account_id.clone(),
                    state.access_token.is_some(),
                    stored,
                );
            }
        };
        let signed_in = in_memory || stored;
        let pending = self
            .inner
            .logins
            .lock()
            .map(|logins| logins.values().any(|entry| entry.provider == provider))
            .unwrap_or(false);
        SubscriptionAccountStatus {
            provider,
            state: if signed_in {
                SubscriptionAccountState::SignedIn
            } else if pending {
                SubscriptionAccountState::Pending
            } else {
                SubscriptionAccountState::SignedOut
            },
            account_id,
            generation,
            message: None,
        }
    }

    async fn start_grok_device_login(
        &self,
    ) -> Result<
        (
            SubscriptionLoginChallenge,
            BoxFuture<'static, Result<TokenResponse, AuthError>>,
        ),
        AuthError,
    > {
        let url = format!("{}/oauth2/device/code", self.inner.endpoints.grok_issuer);
        let response = self
            .inner
            .http
            .post(url)
            .header("x-grok-client-version", env!("CARGO_PKG_VERSION"))
            .header("x-grok-client-surface", "desktop")
            .form(&[
                ("client_id", GROK_CLIENT_ID),
                (
                    "scope",
                    "openid profile email offline_access grok-cli:access api:access",
                ),
            ])
            .send()
            .await
            .map_err(|_| AuthError::Network)?;
        let device: GrokDeviceStart = json_response(response).await?;
        validate_verification_url(&device.verification_uri)?;
        let verification_url = device
            .verification_uri_complete
            .filter(|url| validate_verification_url(url).is_ok())
            .unwrap_or(device.verification_uri);
        let lifetime = Duration::from_secs(device.expires_in.unwrap_or(300).clamp(1, 900));
        let deadline = Instant::now() + lifetime;
        let interval = Duration::from_secs(device.interval.unwrap_or(5).clamp(1, 60));
        let login_id = Uuid::new_v4().to_string();
        let challenge = SubscriptionLoginChallenge {
            kind: LoginKind::DeviceCode,
            login_id,
            verification_url,
            user_code: device.user_code,
            expires_at: epoch_millis_after(lifetime),
        };
        let http = self.inner.http.clone();
        let issuer = self.inner.endpoints.grok_issuer.clone();
        let device_code = device.device_code;
        let future = Box::pin(async move {
            let mut poll_interval = interval;
            loop {
                sleep(poll_interval).await;
                if Instant::now() >= deadline {
                    return Err(AuthError::Expired);
                }
                let response = http
                    .post(format!("{issuer}/oauth2/token"))
                    .form(&[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("client_id", GROK_CLIENT_ID),
                        ("device_code", device_code.as_str()),
                    ])
                    .send()
                    .await
                    .map_err(|_| AuthError::Network)?;
                if response.status().is_success() {
                    return response
                        .json()
                        .await
                        .map_err(|_| AuthError::InvalidResponse);
                }
                let error = oauth_error(response).await;
                match error.as_deref() {
                    Some("authorization_pending") => continue,
                    Some("slow_down") => poll_interval = next_grok_poll_interval(poll_interval),
                    Some("access_denied" | "authorization_denied") => {
                        return Err(AuthError::Denied)
                    }
                    Some("expired_token") => return Err(AuthError::Expired),
                    _ => return Err(AuthError::Network),
                }
            }
        }) as BoxFuture<'static, Result<TokenResponse, AuthError>>;
        Ok((challenge, future))
    }

    async fn start_codex_browser_login(
        &self,
    ) -> Result<
        (
            SubscriptionLoginChallenge,
            BoxFuture<'static, Result<TokenResponse, AuthError>>,
        ),
        AuthError,
    > {
        let listener = TcpListener::bind("127.0.0.1:1455")
            .await
            .map_err(|_| AuthError::Network)?;
        let state = Uuid::new_v4().simple().to_string();
        let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut authorization = Url::parse(&format!(
            "{}/oauth/authorize",
            self.inner.endpoints.codex_issuer
        ))
        .map_err(|_| AuthError::InvalidResponse)?;
        authorization
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CODEX_CLIENT_ID)
            .append_pair("redirect_uri", &self.inner.endpoints.codex_callback)
            .append_pair("scope", "openid profile email offline_access")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        let login_id = Uuid::new_v4().to_string();
        let result_challenge = SubscriptionLoginChallenge {
            kind: LoginKind::BrowserPkce,
            login_id,
            verification_url: authorization.to_string(),
            user_code: String::new(),
            expires_at: epoch_millis_after(MAX_LOGIN_LIFETIME),
        };
        let callback_timeout = MAX_LOGIN_LIFETIME;
        let callback = Box::pin(async move {
            let deadline = Instant::now() + callback_timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(AuthError::Expired);
                }
                let (mut socket, _) = timeout(remaining, listener.accept())
                    .await
                    .map_err(|_| AuthError::Expired)?
                    .map_err(|_| AuthError::Network)?;
                let mut bytes = Vec::with_capacity(1024);
                let mut chunk = [0u8; 1024];
                while bytes.len() < 8192 && !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read_timeout = Duration::from_secs(3)
                        .min(deadline.saturating_duration_since(Instant::now()));
                    let count = match timeout(read_timeout, socket.read(&mut chunk)).await {
                        Ok(Ok(count)) => count,
                        _ => 0,
                    };
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8_lossy(&bytes);
                let mut fields = request
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .split_whitespace();
                let method = fields.next().unwrap_or_default();
                let target = fields.next().unwrap_or_default();
                if method != "GET" || target.is_empty() {
                    respond_callback(&mut socket, 400, "Invalid sign-in callback request.").await;
                    continue;
                }
                let callback_url = match Url::parse(&format!("http://localhost:1455{target}")) {
                    Ok(url) => url,
                    Err(_) => {
                        respond_callback(&mut socket, 400, "Invalid sign-in callback request.")
                            .await;
                        continue;
                    }
                };
                if callback_url.path() != "/auth/callback" {
                    respond_callback(&mut socket, 404, "Sign-in callback not found.").await;
                    continue;
                }
                let params = callback_url
                    .query_pairs()
                    .into_owned()
                    .collect::<HashMap<_, _>>();
                if params.get("state").map(String::as_str) != Some(state.as_str()) {
                    respond_callback(&mut socket, 400, "Sign-in state did not match.").await;
                    continue;
                }
                if let Some(error) = params.get("error") {
                    respond_callback(&mut socket, 400, "Sign-in was denied.").await;
                    return Err(if error == "access_denied" {
                        AuthError::Denied
                    } else {
                        AuthError::InvalidResponse
                    });
                }
                let Some(code) = params.get("code").cloned() else {
                    respond_callback(&mut socket, 400, "Sign-in callback did not include a code.")
                        .await;
                    continue;
                };
                respond_callback(
                    &mut socket,
                    200,
                    "Authorization received. Return to Studio to finish sign-in.",
                )
                .await;
                return Ok((code, verifier));
            }
        });
        let issuer = self.inner.endpoints.codex_issuer.clone();
        let redirect_uri = self.inner.endpoints.codex_callback.clone();
        let http = self.inner.http.clone();
        let future = Box::pin(async move {
            let (code, verifier) = callback.await?;
            let response = http
                .post(format!("{issuer}/oauth/token"))
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", CODEX_CLIENT_ID),
                    ("code", code.as_str()),
                    ("redirect_uri", redirect_uri.as_str()),
                    ("code_verifier", verifier.as_str()),
                ])
                .send()
                .await
                .map_err(|_| AuthError::Network)?;
            json_response(response).await
        }) as BoxFuture<'static, Result<TokenResponse, AuthError>>;
        Ok((result_challenge, future))
    }

    async fn start_codex_device_login(
        &self,
    ) -> Result<
        (
            SubscriptionLoginChallenge,
            BoxFuture<'static, Result<TokenResponse, AuthError>>,
        ),
        AuthError,
    > {
        let issuer = self.inner.endpoints.codex_issuer.clone();
        let response = self
            .inner
            .http
            .post(format!("{issuer}/api/accounts/deviceauth/usercode"))
            .json(&serde_json::json!({"client_id": CODEX_CLIENT_ID}))
            .send()
            .await
            .map_err(|_| AuthError::Network)?;
        let device: CodexDeviceStart = json_response(response).await?;
        let lifetime = Duration::from_secs(device.expires_in.unwrap_or(300).clamp(1, 900));
        let deadline = Instant::now() + lifetime;
        let device_url = format!("{issuer}/codex/device");
        let challenge = SubscriptionLoginChallenge {
            kind: LoginKind::DeviceCode,
            login_id: Uuid::new_v4().to_string(),
            verification_url: device_url,
            user_code: device.user_code.clone(),
            expires_at: epoch_millis_after(lifetime),
        };
        let http = self.inner.http.clone();
        let device_auth_id = device.device_auth_id;
        let user_code = device.user_code;
        let interval = Duration::from_secs(device.interval.unwrap_or(5).clamp(1, 30));
        let future = Box::pin(async move {
            loop {
                sleep(interval).await;
                if Instant::now() >= deadline {
                    return Err(AuthError::Expired);
                }
                let response = http.post(format!("{issuer}/api/accounts/deviceauth/token"))
                    .json(&serde_json::json!({"device_auth_id": device_auth_id, "user_code": user_code}))
                    .send().await.map_err(|_| AuthError::Network)?;
                if response.status() == StatusCode::FORBIDDEN
                    || response.status() == StatusCode::NOT_FOUND
                {
                    continue;
                }
                let poll: CodexDevicePoll = json_response(response).await?;
                let code = poll.authorization_code.ok_or(AuthError::InvalidResponse)?;
                let verifier = poll.code_verifier.ok_or(AuthError::InvalidResponse)?;
                let token_response = http
                    .post(format!("{issuer}/oauth/token"))
                    .form(&[
                        ("grant_type", "authorization_code"),
                        ("client_id", CODEX_CLIENT_ID),
                        ("code", code.as_str()),
                        (
                            "redirect_uri",
                            "https://auth.openai.com/deviceauth/callback",
                        ),
                        ("code_verifier", verifier.as_str()),
                    ])
                    .send()
                    .await
                    .map_err(|_| AuthError::Network)?;
                return json_response(token_response).await;
            }
        }) as BoxFuture<'static, Result<TokenResponse, AuthError>>;
        Ok((challenge, future))
    }

    async fn install_tokens(
        &self,
        provider: SubscriptionProvider,
        expected_generation: u64,
        login_id: &str,
        cancellation: &CancellationToken,
        tokens: TokenResponse,
    ) -> Result<(), AuthError> {
        if tokens.access_token.is_empty() {
            return Err(AuthError::InvalidResponse);
        }
        let account_id = token_account_id(provider, &tokens);
        let refresh_token = tokens
            .refresh_token
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or(AuthError::InvalidResponse)?;
        let encoded = serde_json::to_string(&StoredCredential {
            refresh_token,
            account_id: account_id.clone(),
        })
        .map_err(|_| AuthError::InvalidResponse)?;
        let mut state = self.state(provider).lock().await;
        if state.generation != expected_generation || cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        if !self
            .inner
            .logins
            .lock()
            .map_err(|_| AuthError::InvalidResponse)?
            .get(login_id)
            .is_some_and(|pending| {
                pending.provider == provider && !pending.cancellation.is_cancelled()
            })
        {
            return Err(AuthError::Cancelled);
        }
        self.write_stored(provider, encoded).await?;
        state.generation = state.generation.wrapping_add(1);
        state.access_token = Some(tokens.access_token);
        state.account_id = account_id;
        state.access_expires_at = Some(
            Instant::now()
                + Duration::from_secs(tokens.expires_in.unwrap_or(3600).saturating_sub(30)),
        );
        Ok(())
    }

    async fn read_stored(
        &self,
        provider: SubscriptionProvider,
    ) -> Result<Option<StoredCredential>, AuthError> {
        let store = self.inner.store.clone();
        let value = tokio::task::spawn_blocking(move || store.read(provider))
            .await
            .map_err(|_| AuthError::StorageUnavailable)?
            .map_err(|_| AuthError::StorageUnavailable)?;
        value
            .map(|value| serde_json::from_str(&value).map_err(|_| AuthError::StorageUnavailable))
            .transpose()
    }

    async fn write_stored(
        &self,
        provider: SubscriptionProvider,
        value: String,
    ) -> Result<(), AuthError> {
        let store = self.inner.store.clone();
        tokio::task::spawn_blocking(move || store.write(provider, &value))
            .await
            .map_err(|_| AuthError::StorageUnavailable)?
            .map_err(|_| AuthError::StorageUnavailable)
    }

    async fn delete_stored(&self, provider: SubscriptionProvider) -> Result<(), AuthError> {
        let store = self.inner.store.clone();
        tokio::task::spawn_blocking(move || store.delete(provider))
            .await
            .map_err(|_| AuthError::StorageUnavailable)?
            .map_err(|_| AuthError::StorageUnavailable)
    }

    async fn refresh_locked(
        &self,
        provider: SubscriptionProvider,
        expected_generation: u64,
        used_access_token: Option<&str>,
    ) -> Result<AuthorizedSession, SessionError> {
        let _refresh = self.inner.refresh_locks[provider_index(provider)]
            .lock()
            .await;
        let (generation, existing_access, existing_expiry) = {
            let state = self.state(provider).lock().await;
            if state.generation != expected_generation {
                return Err(SessionError::StaleGeneration);
            }
            (
                state.generation,
                state.access_token.clone(),
                state.access_expires_at,
            )
        };
        if let (Some(access), Some(expiry)) = (&existing_access, existing_expiry) {
            let still_valid = expiry > Instant::now();
            let different_from_used = used_access_token
                .map(|used| used != access)
                .unwrap_or(false);
            if still_valid && (used_access_token.is_none() || different_from_used) {
                let state = self.state(provider).lock().await;
                if state.generation != generation {
                    return Err(SessionError::StaleGeneration);
                }
                return session_from_state(&state);
            }
        }
        let stored = self
            .read_stored(provider)
            .await
            .map_err(|_| SessionError::StorageUnavailable)?
            .ok_or(SessionError::SignedOut)?;
        let refreshed = self
            .exchange_refresh(provider, &stored.refresh_token)
            .await
            .map_err(|_| SessionError::RefreshRejected)?;
        if refreshed.access_token.trim().is_empty()
            || refreshed
                .refresh_token
                .as_ref()
                .is_some_and(|token| token.trim().is_empty())
        {
            return Err(SessionError::RefreshRejected);
        }
        let refresh_token = refreshed
            .refresh_token
            .clone()
            .unwrap_or(stored.refresh_token);
        let account_id = token_account_id(provider, &refreshed).or(stored.account_id);
        let encoded = serde_json::to_string(&StoredCredential {
            refresh_token,
            account_id: account_id.clone(),
        })
        .map_err(|_| SessionError::StorageUnavailable)?;
        let mut state = self.state(provider).lock().await;
        if state.generation != expected_generation {
            return Err(SessionError::StaleGeneration);
        }
        self.write_stored(provider, encoded)
            .await
            .map_err(|_| SessionError::StorageUnavailable)?;
        state.access_token = Some(refreshed.access_token);
        state.account_id = account_id;
        state.access_expires_at = Some(
            Instant::now()
                + Duration::from_secs(refreshed.expires_in.unwrap_or(3600).saturating_sub(30)),
        );
        session_from_state(&state)
    }

    async fn exchange_refresh(
        &self,
        provider: SubscriptionProvider,
        refresh_token: &str,
    ) -> Result<TokenResponse, AuthError> {
        let (url, client_id) = match provider {
            SubscriptionProvider::CodexSubscription => (
                format!("{}/oauth/token", self.inner.endpoints.codex_issuer),
                CODEX_CLIENT_ID,
            ),
            SubscriptionProvider::GrokSubscription => (
                format!("{}/oauth2/token", self.inner.endpoints.grok_issuer),
                GROK_CLIENT_ID,
            ),
        };
        let response = self
            .inner
            .http
            .post(url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .map_err(|_| AuthError::Network)?;
        json_response(response).await
    }
}

impl SubscriptionSessions for NativeSubscriptionAuth {
    fn authorized(
        &self,
        provider: SubscriptionProvider,
        expected_generation: u64,
    ) -> impl Future<Output = Result<AuthorizedSession, SessionError>> + Send {
        self.refresh_locked(provider, expected_generation, None)
    }

    fn refresh_after_unauthorized(
        &self,
        provider: SubscriptionProvider,
        expected_generation: u64,
        used_access_token: &str,
    ) -> impl Future<Output = Result<AuthorizedSession, SessionError>> + Send {
        self.refresh_locked(provider, expected_generation, Some(used_access_token))
    }

    async fn generation(&self, provider: SubscriptionProvider) -> u64 {
        self.state(provider).lock().await.generation
    }

    async fn sign_out(&self, provider: SubscriptionProvider) -> Result<u64, SessionError> {
        let mut state = self.state(provider).lock().await;
        state.generation = state.generation.wrapping_add(1);
        state.access_token = None;
        state.account_id = None;
        state.access_expires_at = None;
        let generation = state.generation;
        self.cancel_provider_logins(provider)
            .map_err(|_| SessionError::StorageUnavailable)?;
        self.delete_stored(provider)
            .await
            .map_err(|_| SessionError::StorageUnavailable)?;
        drop(state);
        Ok(generation)
    }
}

impl NativeSubscriptionAuth {
    fn cancel_provider_logins(&self, provider: SubscriptionProvider) -> Result<(), ()> {
        let mut logins = self.inner.logins.lock().map_err(|_| ())?;
        let ids = logins
            .iter()
            .filter_map(|(id, pending)| (pending.provider == provider).then_some(id.clone()))
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(pending) = logins.remove(&id) {
                pending.cancellation.cancel();
            }
        }
        Ok(())
    }
}

fn provider_index(provider: SubscriptionProvider) -> usize {
    match provider {
        SubscriptionProvider::CodexSubscription => 0,
        SubscriptionProvider::GrokSubscription => 1,
    }
}

fn session_from_state(state: &SessionState) -> Result<AuthorizedSession, SessionError> {
    let access_token = state.access_token.clone().ok_or(SessionError::SignedOut)?;
    Ok(AuthorizedSession {
        access_token,
        account_id: state.account_id.clone(),
        generation: state.generation,
    })
}

fn next_grok_poll_interval(current: Duration) -> Duration {
    (current + Duration::from_secs(5)).min(Duration::from_secs(30))
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected a nonnegative integer")),
        Some(serde_json::Value::String(number)) => number
            .parse::<u64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(_) => Err(serde::de::Error::custom(
            "expected an integer or numeric string",
        )),
    }
}

async fn json_response<T: for<'de> Deserialize<'de>>(response: Response) -> Result<T, AuthError> {
    if !response.status().is_success() {
        let _ = oauth_error(response).await;
        return Err(AuthError::Network);
    }
    response
        .json()
        .await
        .map_err(|_| AuthError::InvalidResponse)
}

async fn oauth_error(response: Response) -> Option<String> {
    response
        .json::<OAuthError>()
        .await
        .ok()
        .and_then(|error| error.error)
}

fn validate_verification_url(value: &str) -> Result<(), AuthError> {
    let url = Url::parse(value).map_err(|_| AuthError::InvalidResponse)?;
    #[cfg(test)]
    if url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost")) {
        return Ok(());
    }
    if url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("auth.x.ai" | "accounts.x.ai" | "auth.openai.com" | "chatgpt.com")
        )
    {
        Ok(())
    } else {
        Err(AuthError::InvalidResponse)
    }
}

async fn respond_callback(socket: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let body = format!("<!doctype html><title>OpenSCAD Studio</title><p>{message}</p>");
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = socket.write_all(response.as_bytes()).await;
}

fn token_account_id(provider: SubscriptionProvider, tokens: &TokenResponse) -> Option<String> {
    let id_token = tokens.id_token.as_deref().unwrap_or(&tokens.access_token);
    let payload = id_token.split('.').nth(1)?;
    let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let codex_account = claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    match provider {
        SubscriptionProvider::CodexSubscription => codex_account,
        SubscriptionProvider::GrokSubscription => codex_account.or_else(|| {
            claims
                .get("sub")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }),
    }
}

fn epoch_millis_after(duration: Duration) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(duration)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::task::JoinHandle;
    use tokio::{net::TcpListener, sync::oneshot};

    #[derive(Default)]
    struct MemoryStore {
        values: StdMutex<HashMap<&'static str, String>>,
        fail_writes: bool,
    }

    impl CredentialStore for MemoryStore {
        fn read(&self, provider: SubscriptionProvider) -> Result<Option<String>, ()> {
            Ok(self
                .values
                .lock()
                .map_err(|_| ())?
                .get(provider_key(provider))
                .cloned())
        }
        fn write(&self, provider: SubscriptionProvider, value: &str) -> Result<(), ()> {
            if self.fail_writes {
                return Err(());
            }
            self.values
                .lock()
                .map_err(|_| ())?
                .insert(provider_key(provider), value.to_owned());
            Ok(())
        }
        fn delete(&self, provider: SubscriptionProvider) -> Result<(), ()> {
            self.values
                .lock()
                .map_err(|_| ())?
                .remove(provider_key(provider));
            Ok(())
        }
    }

    fn stored_refresh(value: &str) -> String {
        serde_json::to_string(&StoredCredential {
            refresh_token: value.into(),
            account_id: None,
        })
        .unwrap()
    }

    fn token_with_claims(claims: serde_json::Value) -> TokenResponse {
        TokenResponse {
            access_token: format!(
                "header.{}.signature",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            ),
            refresh_token: None,
            expires_in: None,
            id_token: None,
        }
    }

    #[test]
    fn account_identity_is_provider_specific() {
        let codex_claims = serde_json::json!({
            "sub": "user-subject",
            "https://api.openai.com/auth": { "chatgpt_account_id": "chatgpt-account" }
        });
        assert_eq!(
            token_account_id(
                SubscriptionProvider::CodexSubscription,
                &token_with_claims(codex_claims)
            ),
            Some("chatgpt-account".into())
        );
        let subject_only = token_with_claims(serde_json::json!({ "sub": "user-subject" }));
        assert_eq!(
            token_account_id(SubscriptionProvider::CodexSubscription, &subject_only),
            None
        );
        assert_eq!(
            token_account_id(SubscriptionProvider::GrokSubscription, &subject_only),
            Some("user-subject".into())
        );
    }

    #[test]
    fn grok_slow_down_adds_five_seconds_and_caps_interval() {
        assert_eq!(
            next_grok_poll_interval(Duration::from_secs(1)),
            Duration::from_secs(6)
        );
        assert_eq!(
            next_grok_poll_interval(Duration::from_secs(27)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_grok_poll_interval(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn codex_device_start_accepts_numeric_or_string_timing_fields() {
        let response: CodexDeviceStart = serde_json::from_str(
            r#"{"device_auth_id":"opaque","user_code":"ABCD","interval":"5","expires_in":"300"}"#,
        )
        .unwrap();
        assert_eq!(response.interval, Some(5));
        assert_eq!(response.expires_in, Some(300));

        let response: CodexDeviceStart = serde_json::from_str(
            r#"{"device_auth_id":"opaque","user_code":"ABCD","interval":5,"expires_in":300}"#,
        )
        .unwrap();
        assert_eq!(response.interval, Some(5));
        assert_eq!(response.expires_in, Some(300));
    }

    #[test]
    fn user_facing_auth_errors_are_fixed_and_do_not_echo_provider_payloads() {
        let errors = [
            AuthError::Cancelled,
            AuthError::Expired,
            AuthError::Denied,
            AuthError::Network,
            AuthError::InvalidResponse,
            AuthError::StorageUnavailable,
            AuthError::AlreadyPending,
        ];
        for error in errors {
            let message = super::super::sanitize_auth_error(error);
            assert!(!message.contains("refresh_token"));
            assert!(!message.contains("access_token"));
            assert!(!message.contains("private response text"));
        }
    }

    #[tokio::test]
    async fn successful_device_login_persists_only_native_refresh_credential() {
        let store = Arc::new(MemoryStore::default());
        let auth =
            NativeSubscriptionAuth::with_test_store(store.clone(), "http://127.0.0.1".into());
        let provider = SubscriptionProvider::GrokSubscription;
        let login_id = "synthetic-success";
        let cancellation = CancellationToken::new();
        auth.inner.logins.lock().unwrap().insert(
            login_id.into(),
            PendingLogin {
                provider,
                cancellation: cancellation.clone(),
            },
        );

        auth.install_tokens(
            provider,
            0,
            login_id,
            &cancellation,
            TokenResponse {
                access_token: "memory-access-token".into(),
                refresh_token: Some("native-refresh-credential".into()),
                expires_in: Some(3600),
                id_token: None,
            },
        )
        .await
        .unwrap();

        let stored: StoredCredential = serde_json::from_str(
            store
                .values
                .lock()
                .unwrap()
                .get(provider_key(provider))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored.refresh_token, "native-refresh-credential");
        assert!(!serde_json::to_string(&stored)
            .unwrap()
            .contains("memory-access-token"));
        let session = auth.authorized(provider, 1).await.unwrap();
        assert_eq!(session.access_token, "memory-access-token");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_backend_round_trips_isolated_synthetic_credential() {
        let service = format!("{KEYRING_SERVICE}.integration-test");
        let account = format!("roundtrip-{}", Uuid::new_v4());
        let entry = Entry::new(&service, &account).unwrap();
        let sentinel = format!("synthetic-{}", Uuid::new_v4());

        entry.set_password(&sentinel).unwrap();
        assert_eq!(entry.get_password().unwrap(), sentinel);
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }

    async fn mock_tokens(
        responses: Vec<&'static str>,
    ) -> (String, Arc<AtomicUsize>, JoinHandle<()>) {
        mock_http_sequence(responses.into_iter().map(|body| ("200 OK", body)).collect()).await
    }

    async fn mock_http_sequence(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (String, Arc<AtomicUsize>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let task = tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (issuer, count, task)
    }

    const FIRST: &str =
        r#"{"access_token":"access-one","refresh_token":"refresh-one","expires_in":3600}"#;
    const ROTATED: &str =
        r#"{"access_token":"access-two","refresh_token":"refresh-two","expires_in":3600}"#;
    const DEVICE_START: &str = r#"{"device_code":"opaque-device","user_code":"ABCD-EFGH","verification_uri":"http://127.0.0.1:9999/verify","expires_in":60,"interval":1}"#;

    async fn send_callback(target: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect("127.0.0.1:1455")
            .await
            .unwrap();
        let request =
            format!("GET {target} HTTP/1.1\r\nHost: localhost:1455\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = vec![0; 1024];
        let count = stream.read(&mut response).await.unwrap();
        String::from_utf8_lossy(&response[..count]).into_owned()
    }

    #[tokio::test]
    async fn restart_restores_refresh_credential_and_rotates_it() {
        let (issuer, count, server) = mock_tokens(vec![FIRST]).await;
        let store = Arc::new(MemoryStore::default());
        store
            .write(
                SubscriptionProvider::GrokSubscription,
                &stored_refresh("refresh-old"),
            )
            .unwrap();
        let auth = NativeSubscriptionAuth::with_test_store(store.clone(), issuer);

        let status = auth.status(SubscriptionProvider::GrokSubscription).await;
        assert!(matches!(status.state, SubscriptionAccountState::SignedIn));
        assert_eq!(status.generation, 0);
        let session = auth
            .authorized(SubscriptionProvider::GrokSubscription, 0)
            .await
            .unwrap();
        assert_eq!(session.access_token, "access-one");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .read(SubscriptionProvider::GrokSubscription)
                .unwrap()
                .unwrap(),
            stored_refresh("refresh-one")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_unauthorized_calls_share_one_rotating_refresh() {
        let (issuer, count, server) = mock_tokens(vec![FIRST, ROTATED]).await;
        let store = Arc::new(MemoryStore::default());
        store
            .write(
                SubscriptionProvider::GrokSubscription,
                &stored_refresh("refresh-old"),
            )
            .unwrap();
        let auth = NativeSubscriptionAuth::with_test_store(store.clone(), issuer);
        let initial = auth
            .authorized(SubscriptionProvider::GrokSubscription, 0)
            .await
            .unwrap();
        let (left, right) = tokio::join!(
            auth.refresh_after_unauthorized(
                SubscriptionProvider::GrokSubscription,
                0,
                &initial.access_token
            ),
            auth.refresh_after_unauthorized(
                SubscriptionProvider::GrokSubscription,
                0,
                &initial.access_token
            ),
        );
        assert_eq!(left.unwrap().access_token, "access-two");
        assert_eq!(right.unwrap().access_token, "access-two");
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(
            store
                .read(SubscriptionProvider::GrokSubscription)
                .unwrap()
                .unwrap(),
            stored_refresh("refresh-two")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sign_out_invalidates_in_flight_restore_before_it_can_persist() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let (accepted, accepted_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let _ = accepted.send(());
            let _ = release_rx.await;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", FIRST.len(), FIRST);
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let store = Arc::new(MemoryStore::default());
        store
            .write(
                SubscriptionProvider::GrokSubscription,
                &stored_refresh("refresh-old"),
            )
            .unwrap();
        let auth = NativeSubscriptionAuth::with_test_store(store.clone(), issuer);
        let restore = {
            let auth = auth.clone();
            tokio::spawn(async move {
                auth.authorized(SubscriptionProvider::GrokSubscription, 0)
                    .await
            })
        };
        accepted_rx.await.unwrap();
        let generation = auth
            .sign_out(SubscriptionProvider::GrokSubscription)
            .await
            .unwrap();
        assert_eq!(generation, 1);
        release.send(()).unwrap();
        assert!(matches!(
            restore.await.unwrap(),
            Err(SessionError::StaleGeneration)
        ));
        assert!(store
            .read(SubscriptionProvider::GrokSubscription)
            .unwrap()
            .is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn storage_failure_never_exposes_a_session() {
        let (issuer, _, server) = mock_tokens(vec![FIRST]).await;
        let store = Arc::new(MemoryStore {
            values: StdMutex::new(HashMap::from([(
                "grok-subscription",
                stored_refresh("refresh-old"),
            )])),
            fail_writes: true,
        });
        let auth = NativeSubscriptionAuth::with_test_store(store, issuer);
        assert!(matches!(
            auth.authorized(SubscriptionProvider::GrokSubscription, 0)
                .await,
            Err(SessionError::StorageUnavailable)
        ));
        assert_eq!(
            auth.generation(SubscriptionProvider::GrokSubscription)
                .await,
            0
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn grok_device_denial_is_terminal_and_never_echoes_provider_text() {
        let (issuer, _, server) = mock_http_sequence(vec![
            ("200 OK", DEVICE_START),
            (
                "400 Bad Request",
                r#"{"error":"access_denied","error_description":"private response text"}"#,
            ),
        ])
        .await;
        let auth =
            NativeSubscriptionAuth::with_test_store(Arc::new(MemoryStore::default()), issuer);
        let (challenge, poll) = auth.start_grok_device_login().await.unwrap();
        assert_eq!(challenge.user_code, "ABCD-EFGH");
        assert!(challenge
            .verification_url
            .starts_with("http://127.0.0.1:9999/"));
        assert!(matches!(poll.await, Err(AuthError::Denied)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn grok_device_expiry_bounds_polling() {
        let body = r#"{"device_code":"opaque-device","user_code":"ABCD-EFGH","verification_uri":"http://127.0.0.1:9999/verify","expires_in":1,"interval":1}"#;
        let (issuer, count, server) = mock_http_sequence(vec![("200 OK", body)]).await;
        let auth =
            NativeSubscriptionAuth::with_test_store(Arc::new(MemoryStore::default()), issuer);
        let (_, poll) = auth.start_grok_device_login().await.unwrap();
        assert!(matches!(poll.await, Err(AuthError::Expired)));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_pending_device_login_clears_pending_state() {
        let (issuer, _, server) = mock_http_sequence(vec![("200 OK", DEVICE_START)]).await;
        let auth =
            NativeSubscriptionAuth::with_test_store(Arc::new(MemoryStore::default()), issuer);
        let challenge = auth
            .start_login(SubscriptionProvider::GrokSubscription)
            .await
            .unwrap();
        server.await.unwrap();
        auth.cancel_login(SubscriptionProvider::GrokSubscription, &challenge.login_id)
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            auth.status(SubscriptionProvider::GrokSubscription)
                .await
                .state,
            SubscriptionAccountState::SignedOut
        ));
        assert!(!auth
            .inner
            .logins
            .lock()
            .unwrap()
            .contains_key(&challenge.login_id));
    }

    #[tokio::test]
    async fn concurrent_device_login_starts_reserve_a_single_provider_slot() {
        let (issuer, count, server) = mock_http_sequence(vec![("200 OK", DEVICE_START)]).await;
        let auth =
            NativeSubscriptionAuth::with_test_store(Arc::new(MemoryStore::default()), issuer);
        let (left, right) = tokio::join!(
            auth.start_login(SubscriptionProvider::GrokSubscription),
            auth.start_login(SubscriptionProvider::GrokSubscription),
        );
        assert!(left.is_ok());
        assert!(
            matches!(&right, Err(AuthError::AlreadyPending))
                || matches!(&left, Err(AuthError::AlreadyPending))
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let challenge = left.ok().or_else(|| right.ok()).unwrap();
        auth.cancel_login(SubscriptionProvider::GrokSubscription, &challenge.login_id)
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn browser_callback_rejects_wrong_state_then_handles_denial() {
        let listener = match TcpListener::bind("127.0.0.1:1455").await {
            Ok(listener) => listener,
            Err(_) => return,
        };
        drop(listener);
        let auth = NativeSubscriptionAuth::with_store(Arc::new(MemoryStore::default())).unwrap();
        let (challenge, callback) = auth.start_codex_browser_login().await.unwrap();
        let authorize = Url::parse(&challenge.verification_url).unwrap();
        let state = authorize
            .query_pairs()
            .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
            .unwrap();
        let wrong_response = send_callback("/auth/callback?code=bad&state=wrong-state").await;
        assert!(wrong_response.starts_with("HTTP/1.1 400"));
        let target = format!("/auth/callback?error=access_denied&state={state}");
        let denial_response = send_callback(&target).await;
        assert!(denial_response.starts_with("HTTP/1.1 400"));
        assert!(matches!(callback.await, Err(AuthError::Denied)));
    }

    #[tokio::test]
    async fn wrong_provider_cannot_cancel_another_pending_login() {
        let cancellation = CancellationToken::new();
        let auth = NativeSubscriptionAuth::with_store(Arc::new(MemoryStore::default())).unwrap();
        auth.inner.logins.lock().unwrap().insert(
            "opaque-login-id".into(),
            PendingLogin {
                provider: SubscriptionProvider::GrokSubscription,
                cancellation: cancellation.clone(),
            },
        );
        auth.cancel_login(SubscriptionProvider::CodexSubscription, "opaque-login-id")
            .await
            .unwrap();
        assert!(!cancellation.is_cancelled());
        assert!(auth
            .inner
            .logins
            .lock()
            .unwrap()
            .contains_key("opaque-login-id"));
    }
}
