use super::{
    ApiBackend, AuthorizedSession, CapabilitySupport, SessionError, SubscriptionModelInfo,
    SubscriptionProvider, SubscriptionRequest, SubscriptionRequestEvent, SubscriptionSessions,
};
use futures::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tauri::ipc::Channel;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models?client_version=1.0.0";
const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const GROK_MODELS_URL: &str = "https://cli-chat-proxy.grok.com/v1/models";
const GROK_CHAT_URL: &str = "https://cli-chat-proxy.grok.com/v1/chat/completions";
const GROK_RESPONSES_URL: &str = "https://cli-chat-proxy.grok.com/v1/responses";
const STUDIO_INSTRUCTIONS: &str = "You are the OpenSCAD Studio assistant. Help the user build precise OpenSCAD projects. Use the provided tools for project inspection and validated edits. Preserve the user's intent and explain important design decisions.";

type CatalogKey = (SubscriptionProvider, u64);
type RequestKey = (String, String);

#[derive(Clone)]
struct ProviderEndpoints {
    codex_models: String,
    codex_responses: String,
    grok_models: String,
    grok_chat: String,
    grok_responses: String,
}

impl Default for ProviderEndpoints {
    fn default() -> Self {
        Self {
            codex_models: CODEX_MODELS_URL.to_owned(),
            codex_responses: CODEX_RESPONSES_URL.to_owned(),
            grok_models: GROK_MODELS_URL.to_owned(),
            grok_chat: GROK_CHAT_URL.to_owned(),
            grok_responses: GROK_RESPONSES_URL.to_owned(),
        }
    }
}

struct ActiveRequest {
    provider: SubscriptionProvider,
    cancellation: CancellationToken,
}

/// Native provider catalog and streaming transport. Provider URLs and authorization are fixed
/// here; frontend input contributes only a validated model, protocol body, and scoped generation.
pub struct SubscriptionTransport<S: SubscriptionSessions + 'static> {
    sessions: Arc<S>,
    client: Client,
    endpoints: ProviderEndpoints,
    catalog: Mutex<HashMap<CatalogKey, Vec<SubscriptionModelInfo>>>,
    active: Mutex<HashMap<RequestKey, ActiveRequest>>,
}

impl<S: SubscriptionSessions + 'static> SubscriptionTransport<S> {
    pub fn new(sessions: Arc<S>) -> Result<Self, String> {
        Self::with_endpoints(sessions, ProviderEndpoints::default())
    }

    fn with_endpoints(sessions: Arc<S>, endpoints: ProviderEndpoints) -> Result<Self, String> {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("OpenSCAD-Studio/1.0")
            .build()
            .map_err(|_| "Could not initialize the subscription connection.".to_owned())?;
        Ok(Self {
            sessions,
            client,
            endpoints,
            catalog: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
        })
    }

    pub async fn list_models(
        &self,
        provider: SubscriptionProvider,
        generation: u64,
    ) -> Result<Vec<SubscriptionModelInfo>, String> {
        let key = (provider, generation);
        if let Some(models) = self.catalog.lock().await.get(&key).cloned() {
            return Ok(models);
        }

        let mut authorized = self
            .sessions
            .authorized(provider, generation)
            .await
            .map_err(sanitize_session_error)?;
        let mut response = self
            .send_models(provider, &authorized)
            .await
            .map_err(|_| "Could not load subscription models.".to_owned())?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let old_token = authorized.access_token.clone();
            authorized = self
                .sessions
                .refresh_after_unauthorized(provider, generation, &old_token)
                .await
                .map_err(sanitize_session_error)?;
            response = self
                .send_models(provider, &authorized)
                .await
                .map_err(|_| "Could not load subscription models.".to_owned())?;
        }
        if !response.status().is_success() {
            return Err(sanitize_status(response.status()).to_owned());
        }

        let payload: Value = response
            .json()
            .await
            .map_err(|_| "The subscription returned an unreadable model list.".to_owned())?;
        let models = parse_model_catalog(provider, &payload);
        if models.is_empty() {
            return Err("No models are available for this subscription account.".to_owned());
        }
        if self.sessions.generation(provider).await != generation {
            return Err("The subscription account changed. Refresh the model list.".to_owned());
        }
        self.catalog.lock().await.insert(key, models.clone());
        Ok(models)
    }

    pub async fn start_request(
        self: &Arc<Self>,
        window_label: String,
        request: SubscriptionRequest,
        channel: Channel<SubscriptionRequestEvent>,
    ) -> Result<(), String> {
        if request.request_id.is_empty() || request.request_id.len() > 128 {
            return Err("The subscription request ID is invalid.".to_owned());
        }
        if !request.body.is_object() {
            return Err("The subscription request body is invalid.".to_owned());
        }
        if request.continuation_id.is_some()
            || request.body.get("previous_response_id").is_some()
            || request.body.get("conversation_id").is_some()
        {
            return Err(
                "This subscription continuation is not valid for the active account.".to_owned(),
            );
        }
        let cancellation = CancellationToken::new();
        let key = (window_label.clone(), request.request_id.clone());
        {
            let mut active = self.active.lock().await;
            if active.contains_key(&key) {
                return Err("A subscription request with this ID is already active.".to_owned());
            }
            active.insert(
                key.clone(),
                ActiveRequest {
                    provider: request.provider,
                    cancellation: cancellation.clone(),
                },
            );
        }
        let transport = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            transport
                .run_request(window_label, request.clone(), channel, cancellation)
                .await;
            transport.active.lock().await.remove(&key);
        });
        Ok(())
    }

    pub async fn cancel_request(&self, window_label: &str, request_id: &str) {
        if let Some(token) = self
            .active
            .lock()
            .await
            .get(&(window_label.to_owned(), request_id.to_owned()))
        {
            token.cancellation.cancel();
        }
    }

    pub async fn cancel_window(&self, window_label: &str) {
        let active = self.active.lock().await;
        for ((owner, _), request) in active.iter() {
            if owner == window_label {
                request.cancellation.cancel();
            }
        }
    }

    pub async fn cancel_provider(&self, provider: SubscriptionProvider) {
        for request in self.active.lock().await.values() {
            if request.provider == provider {
                request.cancellation.cancel();
            }
        }
    }

    pub async fn invalidate_catalog(&self, provider: SubscriptionProvider) {
        self.catalog
            .lock()
            .await
            .retain(|(cached_provider, _), _| *cached_provider != provider);
    }

    async fn send_models(
        &self,
        provider: SubscriptionProvider,
        session: &AuthorizedSession,
    ) -> Result<Response, reqwest::Error> {
        let mut request = self
            .client
            .get(match provider {
                SubscriptionProvider::CodexSubscription => &self.endpoints.codex_models,
                SubscriptionProvider::GrokSubscription => &self.endpoints.grok_models,
            })
            // Catalog calls are finite and must not pin a cancelled request
            // indefinitely. Streaming chat deliberately has no total timeout.
            .timeout(std::time::Duration::from_secs(30))
            .bearer_auth(&session.access_token)
            .header(ACCEPT, "application/json")
            .header("connection", "close");
        request = add_provider_headers(request, provider, None, session);
        request.send().await
    }

    async fn run_request(
        &self,
        window_label: String,
        request: SubscriptionRequest,
        channel: Channel<SubscriptionRequestEvent>,
        cancellation: CancellationToken,
    ) {
        let result = self
            .run_request_inner(&window_label, &request, &channel, &cancellation)
            .await;
        if let Err(message) = result {
            let _ = send_error(&channel, &request.request_id, 0, &message);
        }
    }

    async fn run_request_inner(
        &self,
        _window_label: &str,
        request: &SubscriptionRequest,
        channel: &Channel<SubscriptionRequestEvent>,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let generation = self.sessions.generation(request.provider).await;
        if generation != request.account_generation {
            return Err("The subscription account changed. Sign in and retry.".to_owned());
        }
        // Authorization may rotate and persist a refresh token. Never drop that
        // future on cancellation; auth calls have a bounded timeout internally.
        let models = self
            .list_models(request.provider, request.account_generation)
            .await?;
        if cancellation.is_cancelled() {
            let _ = send_error(
                channel,
                &request.request_id,
                0,
                "The subscription request was cancelled.",
            );
            return Ok(());
        }
        let model = models
            .iter()
            .find(|model| model.id == request.model_id)
            .ok_or_else(|| {
                "This model is not available in the signed-in subscription.".to_owned()
            })?;
        if matches!(model.api_backend, ApiBackend::Unknown) {
            return Err("This subscription model uses an unsupported request format.".to_owned());
        }

        let body = prepare_body(request, model.api_backend)?;
        let mut session = self
            .sessions
            .authorized(request.provider, request.account_generation)
            .await
            .map_err(sanitize_session_error)?;
        if cancellation.is_cancelled() {
            let _ = send_error(
                channel,
                &request.request_id,
                0,
                "The subscription request was cancelled.",
            );
            return Ok(());
        }
        if session.generation != request.account_generation {
            return Err("The subscription account changed. Sign in and retry.".to_owned());
        }
        let mut response = tokio::select! {
            _ = cancellation.cancelled() => {
                let _ = send_error(channel, &request.request_id, 0, "The subscription request was cancelled.");
                return Ok(());
            },
            result = self.send_chat(request, model.api_backend, &body, &session) => result?,
        };
        if response.status() == StatusCode::UNAUTHORIZED {
            let used_token = session.access_token.clone();
            // Refresh is a rotation transaction. Let it persist atomically even
            // if the request was cancelled while the provider returned 401.
            session = self
                .sessions
                .refresh_after_unauthorized(
                    request.provider,
                    request.account_generation,
                    &used_token,
                )
                .await
                .map_err(sanitize_session_error)?;
            if cancellation.is_cancelled() {
                let _ = send_error(
                    channel,
                    &request.request_id,
                    0,
                    "The subscription request was cancelled.",
                );
                return Ok(());
            }
            if session.generation != request.account_generation
                || self.sessions.generation(request.provider).await != request.account_generation
            {
                return Err("The subscription account changed. Sign in and retry.".to_owned());
            }
            response = tokio::select! {
                _ = cancellation.cancelled() => {
                    let _ = send_error(channel, &request.request_id, 0, "The subscription request was cancelled.");
                    return Ok(());
                },
                result = self.send_chat(request, model.api_backend, &body, &session) => result?,
            };
        }

        let status = response.status();
        let mut safe_headers = BTreeMap::new();
        if let Some(content_type) = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
        {
            safe_headers.insert("content-type".to_owned(), content_type.to_owned());
        }
        send_event(
            channel,
            SubscriptionRequestEvent::Response {
                request_id: request.request_id.clone(),
                sequence: 0,
                status: status.as_u16(),
                headers: safe_headers,
            },
        )?;

        if !status.is_success() {
            let message = sanitize_status(status);
            send_error(channel, &request.request_id, 1, message)?;
            return Ok(());
        }

        let mut chunks = response.bytes_stream();
        let mut sequence = 1;
        loop {
            let next_chunk = tokio::select! {
                _ = cancellation.cancelled() => {
                    let _ = send_error(channel, &request.request_id, sequence, "The subscription request was cancelled.");
                    return Ok(());
                },
                item = chunks.next() => item,
            };
            let Some(next_chunk) = next_chunk else { break };
            let bytes = match next_chunk {
                Ok(bytes) => bytes,
                Err(_) => {
                    let _ = send_error(
                        channel,
                        &request.request_id,
                        sequence,
                        "The subscription stream was interrupted.",
                    );
                    return Ok(());
                }
            };
            if self.sessions.generation(request.provider).await != request.account_generation {
                let _ = send_error(
                    channel,
                    &request.request_id,
                    sequence,
                    "The subscription account changed during the request.",
                );
                return Ok(());
            }
            send_event(
                channel,
                SubscriptionRequestEvent::Chunk {
                    request_id: request.request_id.clone(),
                    sequence,
                    bytes: bytes.to_vec(),
                },
            )?;
            sequence += 1;
        }
        if cancellation.is_cancelled() {
            let _ = send_error(
                channel,
                &request.request_id,
                sequence,
                "The subscription request was cancelled.",
            );
            return Ok(());
        }
        if self.sessions.generation(request.provider).await != request.account_generation {
            let _ = send_error(
                channel,
                &request.request_id,
                sequence,
                "The subscription account changed during the request.",
            );
            return Ok(());
        }
        send_event(
            channel,
            SubscriptionRequestEvent::Complete {
                request_id: request.request_id.clone(),
                sequence,
            },
        )
    }

    async fn send_chat(
        &self,
        request: &SubscriptionRequest,
        backend: ApiBackend,
        body: &Value,
        session: &AuthorizedSession,
    ) -> Result<Response, String> {
        let url = match (request.provider, backend) {
            (SubscriptionProvider::CodexSubscription, ApiBackend::Responses) => {
                &self.endpoints.codex_responses
            }
            (SubscriptionProvider::GrokSubscription, ApiBackend::Responses) => {
                &self.endpoints.grok_responses
            }
            (SubscriptionProvider::GrokSubscription, ApiBackend::ChatCompletions) => {
                &self.endpoints.grok_chat
            }
            _ => {
                return Err(
                    "This subscription model uses an unsupported request format.".to_owned(),
                )
            }
        };
        let mut builder = self
            .client
            .request(Method::POST, url)
            .bearer_auth(&session.access_token)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .header("connection", "close")
            .json(body);
        builder = add_provider_headers(builder, request.provider, Some(&request.model_id), session);
        builder
            .send()
            .await
            .map_err(|_| "Could not connect to the subscription provider.".to_owned())
    }
}

fn add_provider_headers(
    request: RequestBuilder,
    provider: SubscriptionProvider,
    model_id: Option<&str>,
    session: &AuthorizedSession,
) -> RequestBuilder {
    match provider {
        SubscriptionProvider::CodexSubscription => request
            .header("openai-beta", "responses=experimental")
            .header("originator", "openscad-studio")
            .header(
                "chatgpt-account-id",
                session.account_id.as_deref().unwrap_or_default(),
            ),
        SubscriptionProvider::GrokSubscription => {
            let request = request.header("X-XAI-Token-Auth", "xai-grok-cli");
            if let Some(model_id) = model_id {
                request.header("x-grok-model-override", model_id)
            } else {
                request
            }
        }
    }
}

fn prepare_body(request: &SubscriptionRequest, backend: ApiBackend) -> Result<Value, String> {
    let mut body = request.body.clone();
    let object = body
        .as_object_mut()
        .ok_or_else(|| "The subscription request body is invalid.".to_owned())?;
    object.insert("model".to_owned(), Value::String(request.model_id.clone()));
    object.insert("stream".to_owned(), Value::Bool(true));
    match (request.provider, backend) {
        (SubscriptionProvider::CodexSubscription, ApiBackend::Responses) => {
            object.insert("store".to_owned(), Value::Bool(false));
            let instructions = object
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or_default();
            object.insert(
                "instructions".to_owned(),
                Value::String(if instructions.trim().is_empty() {
                    STUDIO_INSTRUCTIONS.to_owned()
                } else {
                    format!("{STUDIO_INSTRUCTIONS}\n\n{instructions}")
                }),
            );
        }
        (SubscriptionProvider::GrokSubscription, ApiBackend::Responses) => {}
        (SubscriptionProvider::GrokSubscription, ApiBackend::ChatCompletions) => {}
        _ => return Err("This subscription model uses an unsupported request format.".to_owned()),
    }
    Ok(body)
}

fn parse_model_catalog(
    provider: SubscriptionProvider,
    payload: &Value,
) -> Vec<SubscriptionModelInfo> {
    let entries = payload
        .get("data")
        .or_else(|| payload.get("models"))
        .and_then(Value::as_array);
    let Some(entries) = entries else {
        return Vec::new();
    };
    let mut models = entries
        .iter()
        .filter_map(|entry| parse_model(provider, entry))
        .collect::<Vec<_>>();
    if provider == SubscriptionProvider::CodexSubscription {
        models.sort_by_key(|model| {
            entries
                .iter()
                .find(|entry| {
                    model.id
                        == string_field(entry, &["slug", "id", "model_id", "model"])
                            .unwrap_or_default()
                })
                .and_then(|entry| entry.get("priority"))
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX)
        });
        if let Some(model) = models.first_mut() {
            model.recommended = true;
        }
    }
    models
}

fn parse_model(provider: SubscriptionProvider, entry: &Value) -> Option<SubscriptionModelInfo> {
    let id = string_field(entry, &["slug", "id", "model_id", "model"])?;
    if id.is_empty() || id.len() > 160 || id.chars().any(char::is_control) {
        return None;
    }
    if provider == SubscriptionProvider::CodexSubscription
        && string_field(entry, &["visibility"]).as_deref() != Some("list")
    {
        return None;
    }
    let modalities = entry
        .get("input_modalities")
        .or_else(|| entry.get("modalities"))
        .and_then(Value::as_array);
    let images = modalities.map_or(CapabilitySupport::Unknown, |modalities| {
        if modalities
            .iter()
            .any(|value| value.as_str() == Some("image"))
        {
            CapabilitySupport::Supported
        } else {
            CapabilitySupport::Unsupported
        }
    });
    let reasoning = entry
        .get("supported_reasoning_levels")
        .or_else(|| entry.get("reasoning"))
        .map_or(CapabilitySupport::Unknown, |value| {
            if value.as_array().is_some_and(|levels| !levels.is_empty())
                || value.as_bool() == Some(true)
            {
                CapabilitySupport::Supported
            } else {
                CapabilitySupport::Unsupported
            }
        });
    let tools = entry
        .get("supports_tools")
        .or_else(|| entry.pointer("/capabilities/tools"))
        .map_or(CapabilitySupport::Unknown, |value| {
            if value.as_bool() == Some(true) {
                CapabilitySupport::Supported
            } else {
                CapabilitySupport::Unsupported
            }
        });
    let api_backend = match string_field(entry, &["api_backend", "apiBackend"]).as_deref() {
        Some("responses") => ApiBackend::Responses,
        Some("chat" | "chat-completions" | "chat_completions") => ApiBackend::ChatCompletions,
        _ if provider == SubscriptionProvider::CodexSubscription => ApiBackend::Responses,
        _ => ApiBackend::Unknown,
    };
    let context_window = entry
        .get("context_window")
        .or_else(|| entry.get("context_length"))
        .and_then(Value::as_u64)
        .and_then(|size| u32::try_from(size).ok());
    Some(SubscriptionModelInfo {
        id: id.clone(),
        name: string_field(entry, &["display_name", "name"]).unwrap_or_else(|| id.clone()),
        api_backend,
        images,
        reasoning,
        tools,
        recommended: entry
            .get("recommended")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        context_window,
    })
}

fn string_field(entry: &Value, fields: &[&str]) -> Option<String> {
    fields
        .iter()
        .find_map(|field| entry.get(*field).and_then(Value::as_str))
        .map(str::to_owned)
}

pub(crate) fn sanitize_session_error(error: SessionError) -> String {
    match error {
        SessionError::SignedOut => "Sign in to this subscription provider first.".to_owned(),
        SessionError::StaleGeneration => {
            "The subscription account changed. Sign in and retry.".to_owned()
        }
        SessionError::RefreshRejected => "Subscription sign-in expired. Sign in again.".to_owned(),
        SessionError::StorageUnavailable => {
            "The operating system could not access the subscription keychain.".to_owned()
        }
    }
}

fn sanitize_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "Subscription sign-in expired. Sign in again.",
        StatusCode::FORBIDDEN => "This subscription does not include access to the selected model.",
        StatusCode::TOO_MANY_REQUESTS => "The subscription provider is busy or its quota is temporarily unavailable. Try again later.",
        StatusCode::NOT_FOUND => "This subscription model is no longer available.",
        _ if status.is_server_error() => "The subscription provider is temporarily unavailable.",
        _ => "The subscription provider rejected this request.",
    }
}

fn send_event(
    channel: &Channel<SubscriptionRequestEvent>,
    event: SubscriptionRequestEvent,
) -> Result<(), String> {
    channel
        .send(event)
        .map_err(|_| "The subscription response window is no longer available.".to_owned())
}

fn send_error(
    channel: &Channel<SubscriptionRequestEvent>,
    request_id: &str,
    sequence: u64,
    message: &str,
) -> Result<(), String> {
    send_event(
        channel,
        SubscriptionRequestEvent::Error {
            request_id: request_id.to_owned(),
            sequence,
            message: message.to_owned(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    struct TestSessions {
        generation: AtomicU64,
    }

    impl SubscriptionSessions for TestSessions {
        fn authorized(
            &self,
            _provider: SubscriptionProvider,
            expected_generation: u64,
        ) -> impl std::future::Future<Output = Result<AuthorizedSession, SessionError>> + Send
        {
            async move {
                if self.generation.load(Ordering::SeqCst) != expected_generation {
                    return Err(SessionError::StaleGeneration);
                }
                Ok(AuthorizedSession {
                    access_token: "test-token".to_owned(),
                    account_id: Some("test-account".to_owned()),
                    generation: expected_generation,
                })
            }
        }

        fn refresh_after_unauthorized(
            &self,
            _provider: SubscriptionProvider,
            expected_generation: u64,
            _used_access_token: &str,
        ) -> impl std::future::Future<Output = Result<AuthorizedSession, SessionError>> + Send
        {
            async move {
                Ok(AuthorizedSession {
                    access_token: "rotated-test-token".to_owned(),
                    account_id: Some("test-account".to_owned()),
                    generation: expected_generation,
                })
            }
        }

        fn generation(
            &self,
            _provider: SubscriptionProvider,
        ) -> impl std::future::Future<Output = u64> + Send {
            async move { self.generation.load(Ordering::SeqCst) }
        }

        fn sign_out(
            &self,
            _provider: SubscriptionProvider,
        ) -> impl std::future::Future<Output = Result<u64, SessionError>> + Send {
            async move { Ok(self.generation.fetch_add(1, Ordering::SeqCst) + 1) }
        }
    }

    fn capture_channel() -> (
        Channel<SubscriptionRequestEvent>,
        Arc<std::sync::Mutex<Vec<SubscriptionRequestEvent>>>,
    ) {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let channel = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                let event: SubscriptionRequestEvent = serde_json::from_str(&json).unwrap();
                captured.lock().unwrap().push(event);
            }
            Ok(())
        });
        (channel, events)
    }

    fn make_test_transport(
        sessions: Arc<TestSessions>,
        base_url: &str,
    ) -> Arc<SubscriptionTransport<TestSessions>> {
        let endpoints = ProviderEndpoints {
            codex_models: format!("{base_url}/models"),
            codex_responses: format!("{base_url}/responses"),
            grok_models: format!("{base_url}/models"),
            grok_chat: format!("{base_url}/chat/completions"),
            grok_responses: format!("{base_url}/responses"),
        };
        Arc::new(SubscriptionTransport::with_endpoints(sessions, endpoints).unwrap())
    }

    fn test_request() -> SubscriptionRequest {
        SubscriptionRequest {
            request_id: "request-1".into(),
            provider: SubscriptionProvider::GrokSubscription,
            account_generation: 1,
            model_id: "grok-4.6".into(),
            body: json!({"input":[{"role":"user","content":"hi"}]}),
            continuation_id: None,
        }
    }

    async fn read_http_request(stream: &mut TcpStream) {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 2048];
        let header_end = loop {
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0, "client closed before sending the request");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0, "client closed before sending the request body");
            bytes.extend_from_slice(&chunk[..count]);
        }
    }

    async fn respond_with_catalog(stream: &mut TcpStream) {
        read_http_request(stream).await;
        let body =
            r#"{"data":[{"model":"grok-4.6","api_backend":"responses","context_window":500000}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(), body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    async fn wait_for_event(
        events: &Arc<std::sync::Mutex<Vec<SubscriptionRequestEvent>>>,
        predicate: impl Fn(&SubscriptionRequestEvent) -> bool,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if events.lock().unwrap().iter().any(&predicate) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("expected native subscription event");
    }

    #[test]
    fn codex_responses_body_disables_storage_and_keeps_studio_instructions() {
        let request = SubscriptionRequest {
            request_id: "req-1".into(),
            provider: SubscriptionProvider::CodexSubscription,
            account_generation: 3,
            model_id: "gpt-5-codex".into(),
            body: json!({ "input": [{"role":"user","content":"build a cube"}], "store": true }),
            continuation_id: None,
        };
        let body = prepare_body(&request, ApiBackend::Responses).unwrap();
        assert_eq!(body["model"], "gpt-5-codex");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body["instructions"]
            .as_str()
            .unwrap()
            .contains("OpenSCAD Studio"));
    }

    #[test]
    fn grok_catalog_distinguishes_responses_chat_and_unknown_capabilities() {
        let models = parse_model_catalog(
            SubscriptionProvider::GrokSubscription,
            &json!({"data":[
                {"id":"grok-4.6","api_backend":"responses","context_length":500000,"input_modalities":["text","image"]},
                {"id":"grok-build","api_backend":"chat-completions","supports_tools":true},
                {"id":"future-model"}
            ]}),
        );
        assert_eq!(models[0].api_backend, ApiBackend::Responses);
        assert_eq!(models[0].images, CapabilitySupport::Supported);
        assert_eq!(models[0].reasoning, CapabilitySupport::Unknown);
        assert_eq!(models[0].tools, CapabilitySupport::Unknown);
        assert_eq!(models[0].context_window, Some(500_000));
        assert_eq!(models[1].api_backend, ApiBackend::ChatCompletions);
        assert_eq!(models[2].api_backend, ApiBackend::Unknown);
    }

    #[test]
    fn codex_catalog_filters_hidden_models_and_sorts_by_priority() {
        let models = parse_model_catalog(
            SubscriptionProvider::CodexSubscription,
            &json!({"models":[
                {"slug":"hidden","visibility":"hidden","priority":0},
                {"slug":"later","visibility":"list","priority":4},
                {"slug":"recommended","visibility":"list","priority":1,"input_modalities":["text"]}
            ]}),
        );
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["recommended", "later"]
        );
        assert!(models[0].recommended);
        assert_eq!(models[0].images, CapabilitySupport::Unsupported);
    }

    #[test]
    fn rust_dtos_serialize_to_the_typescript_camel_case_contract() {
        let status = super::super::SubscriptionAccountStatus {
            provider: SubscriptionProvider::GrokSubscription,
            state: super::super::SubscriptionAccountState::SignedIn,
            account_id: None,
            generation: 4,
            message: None,
        };
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({
                "provider":"grok-subscription", "state":"signed-in", "accountId":null,
                "generation":4, "message":null
            })
        );
        let event = SubscriptionRequestEvent::Response {
            request_id: "req".into(),
            sequence: 0,
            status: 200,
            headers: BTreeMap::from([("content-type".into(), "text/event-stream".into())]),
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "kind":"response", "requestId":"req", "sequence":0, "status":200,
                "headers":{"content-type":"text/event-stream"}
            })
        );
    }

    #[tokio::test]
    async fn cancellation_before_provider_headers_emits_one_terminal_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (post_received_tx, post_received_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut models, _) = listener.accept().await.unwrap();
            respond_with_catalog(&mut models).await;
            let (mut request, _) = listener.accept().await.unwrap();
            read_http_request(&mut request).await;
            let _ = post_received_tx.send(());
            let _ = release_rx.await;
        });

        let sessions = Arc::new(TestSessions {
            generation: AtomicU64::new(1),
        });
        let transport = make_test_transport(sessions, &base_url);
        let (channel, events) = capture_channel();
        transport
            .start_request("window-1".into(), test_request(), channel)
            .await
            .unwrap();
        post_received_rx.await.unwrap();
        transport.cancel_request("window-1", "request-1").await;
        wait_for_event(&events, |event| {
            matches!(event, SubscriptionRequestEvent::Error { .. })
        })
        .await;
        let _ = release_tx.send(());

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            SubscriptionRequestEvent::Error { sequence: 0, message, .. }
                if message.contains("cancelled")
        ));
    }

    #[tokio::test]
    async fn account_generation_change_mid_stream_discards_late_chunks_and_terminates_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (first_chunk_tx, first_chunk_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut models, _) = listener.accept().await.unwrap();
            respond_with_catalog(&mut models).await;
            let (mut request, _) = listener.accept().await.unwrap();
            read_http_request(&mut request).await;
            request
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            let first = b"data: first\n\n";
            request
                .write_all(format!("{:X}\r\n", first.len()).as_bytes())
                .await
                .unwrap();
            request.write_all(first).await.unwrap();
            request.write_all(b"\r\n").await.unwrap();
            request.flush().await.unwrap();
            let _ = first_chunk_tx.send(());
            let _ = release_rx.await;
            let late = b"data: late\n\n";
            let _ = request
                .write_all(format!("{:X}\r\n", late.len()).as_bytes())
                .await;
            let _ = request.write_all(late).await;
            let _ = request.write_all(b"\r\n0\r\n\r\n").await;
        });

        let sessions = Arc::new(TestSessions {
            generation: AtomicU64::new(1),
        });
        let transport = make_test_transport(sessions.clone(), &base_url);
        let (channel, events) = capture_channel();
        transport
            .start_request("window-1".into(), test_request(), channel)
            .await
            .unwrap();
        first_chunk_rx.await.unwrap();
        wait_for_event(&events, |event| {
            matches!(event, SubscriptionRequestEvent::Chunk { .. })
        })
        .await;
        sessions.generation.store(2, Ordering::SeqCst);
        let _ = release_tx.send(());
        wait_for_event(&events, |event| {
            matches!(event, SubscriptionRequestEvent::Error { .. })
        })
        .await;

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            SubscriptionRequestEvent::Response { sequence: 0, .. }
        ));
        assert!(matches!(
            events[1],
            SubscriptionRequestEvent::Chunk { sequence: 1, .. }
        ));
        assert!(matches!(
            &events[2],
            SubscriptionRequestEvent::Error { sequence: 2, message, .. }
                if message.contains("account changed")
        ));
    }
}
