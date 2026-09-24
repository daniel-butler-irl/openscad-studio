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
use std::collections::BTreeMap;
use std::future::Future;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionProvider {
    CodexSubscription,
    GrokSubscription,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

    fn generation(
        &self,
        provider: SubscriptionProvider,
    ) -> impl Future<Output = u64> + Send;

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

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilitySupport {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
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
    /// Must be native-issued and scoped to provider, account generation, and window.
    pub continuation_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
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
