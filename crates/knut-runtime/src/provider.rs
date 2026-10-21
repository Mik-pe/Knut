//! Provider configuration and authenticated Chat Completions / Responses transport.
//!
//! Redirects are refused so credentials cannot cross origins. Missing usage
//! stays unknown; interrupted or incomplete inference never becomes an artifact.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Continuation, ContinuationPart, KnutError, Model, ModelCapabilities, ModelIdentity,
    ModelRequest, ModelResponse, ModelStreamEvent, ModelStreamSink, ModelTier, ToolCall, Usage,
};

mod responses;

/// Wire protocol selected independently from model identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransport {
    ChatCompletions,
    Responses,
}

/// Default endpoint for OpenAI-compatible chat completions.
pub const DEFAULT_CHAT_PATH: &str = "/chat/completions";

/// Reasoning-effort levels this adapter can request.
///
/// The value is passed through only when the provider documents it; an
/// unrecognized setting is refused rather than silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }
}

/// Whether an API key is included in a subscription/coding plan or is
/// metered. Never silently switched: the operator chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingPath {
    /// A coding/subscription endpoint: usage may be covered by a plan.
    Plan,
    /// A metered API endpoint.
    Metered,
    /// Not declared by the operator.
    Unknown,
}

/// Configuration for one provider and its wire protocol.
///
/// `Debug` never reveals the key.
#[derive(Clone)]
pub struct ProviderConfig {
    api_key: String,
    /// Origin + path prefix, no trailing slash (for example
    /// `https://api.z.ai/api/coding/paas/v4`).
    base_url: String,
    model: String,
    tier: ModelTier,
    timeout: Duration,
    billing: BillingPath,
    /// Provider-specific reasoning field name, when the provider uses one
    /// (GLM: `reasoning_content`).
    reasoning_field: String,
    /// Whether this provider accepts `reasoning_effort`.
    supports_reasoning_effort: bool,
    /// Requested effort, sent only when supported.
    reasoning_effort: Option<ReasoningEffort>,
    explicit_reasoning_effort: bool,
    /// Whether this provider accepts `stream_options.include_usage`.
    supports_usage_in_stream: bool,
    transport: ProviderTransport,
    chatgpt_client: Option<String>,
    environment_provider: String,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("api_key", &"[redacted]")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("tier", &self.tier)
            .field("timeout", &self.timeout)
            .field("billing", &self.billing)
            .field("transport", &self.transport)
            .field("chatgpt", &self.uses_chatgpt_plan())
            .finish()
    }
}

impl ProviderConfig {
    /// Build a configuration for one endpoint.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let model = model.into();
        let supports_reasoning_effort = matches!(model.as_str(), "glm-5.3" | "glm-5.3-flash")
            && matches!(
                base_url.as_str(),
                "https://api.z.ai/api/coding/paas/v4" | "https://api.z.ai/api/paas/v4"
            );
        let environment_provider = if base_url.starts_with("https://api.z.ai/") {
            "zai"
        } else {
            "chat-completions"
        }
        .to_owned();
        Self {
            api_key: api_key.into(),
            base_url,
            model,
            tier: ModelTier::Reasoner,
            timeout: Duration::from_secs(120),
            billing: BillingPath::Unknown,
            // GLM's documented field; other OpenAI-compatible providers
            // commonly use `reasoning_content` too, and `reasoning` is
            // accepted as an alternate when reported.
            reasoning_field: "reasoning_content".to_owned(),
            supports_reasoning_effort,
            reasoning_effort: supports_reasoning_effort.then_some(ReasoningEffort::Max),
            explicit_reasoning_effort: false,
            supports_usage_in_stream: true,
            transport: ProviderTransport::ChatCompletions,
            chatgpt_client: None,
            environment_provider,
        }
    }

    /// Z.ai GLM coding-plan profile: `glm-5.3-flash` and friends on the
    /// coding endpoint.
    pub fn glm_coding(api_key: impl Into<String>) -> Self {
        Self::new(
            api_key,
            "https://api.z.ai/api/coding/paas/v4",
            "glm-5.3-flash",
        )
        .with_billing(BillingPath::Plan)
        .with_usage_in_stream(true)
    }

    /// GLM on the metered API endpoint.
    pub fn glm_metered(api_key: impl Into<String>) -> Self {
        Self::new(api_key, "https://api.z.ai/api/paas/v4", "glm-5.3-flash")
            .with_billing(BillingPath::Metered)
            .with_usage_in_stream(true)
    }

    /// Ollama Cloud, which speaks the OpenAI-compatible transport.
    pub fn ollama_cloud(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::new(api_key, "https://ollama.com/v1", model).with_billing(BillingPath::Metered)
    }

    /// OpenAI Responses API, including Codex reasoning models.
    pub fn openai(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let mut config = Self::new(api_key, "https://api.openai.com/v1", model)
            .with_transport(ProviderTransport::Responses)
            .with_billing(BillingPath::Metered);
        config.environment_provider = "openai".to_owned();
        config
    }

    pub fn with_transport(mut self, transport: ProviderTransport) -> Self {
        self.transport = transport;
        if transport == ProviderTransport::Responses {
            self.supports_reasoning_effort = self.model.starts_with("gpt-5")
                || self.model.starts_with("gpt-6")
                || self.model.starts_with("o3")
                || self.model.starts_with("o4");
            self.reasoning_effort = None;
        }
        self
    }

    pub fn with_tier(mut self, tier: ModelTier) -> Self {
        self.tier = tier;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_billing(mut self, billing: BillingPath) -> Self {
        self.billing = billing;
        self
    }

    pub fn with_reasoning_effort_support(mut self, supported: bool) -> Self {
        self.supports_reasoning_effort = supported;
        self
    }

    /// Request a reasoning-effort level. Sent only when the endpoint
    /// documents support; the capability report reflects the truth.
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self.explicit_reasoning_effort = true;
        self
    }

    /// Inferred reasoning defaults follow the model; explicit effort must remain supported.
    pub fn with_model(mut self, model: impl Into<String>) -> Result<Self, KnutError> {
        let model = model.into();
        if model.trim().is_empty() || model.len() > 1024 || model.chars().any(char::is_control) {
            return Err(KnutError::Model("Invalid provider model ID".to_owned()));
        }
        let defaults = Self::new("", &self.base_url, &model).with_transport(self.transport);
        self.model = model;
        self.supports_reasoning_effort = defaults.supports_reasoning_effort;
        if !self.explicit_reasoning_effort {
            self.reasoning_effort = defaults.reasoning_effort;
        }
        self.validate_reasoning()?;
        Ok(self)
    }

    pub fn with_usage_in_stream(mut self, supported: bool) -> Self {
        self.supports_usage_in_stream = supported;
        self
    }

    pub fn with_reasoning_field(mut self, field: impl Into<String>) -> Self {
        self.reasoning_field = field.into();
        self
    }

    /// Build from the environment.
    ///
    /// A saved ChatGPT setting takes priority over environment configuration.
    /// Otherwise `KNUT_PROVIDER` selects the credential source and transport. OpenAI
    /// API credentials come from `OPENAI_API_KEY` or `KNUT_PROVIDER_API_KEY`;
    /// `openai-codex` uses the selected protected ChatGPT registration.
    pub fn from_env() -> Result<Self, KnutError> {
        if let Some(model) = crate::openai_auth::saved_model()? {
            return Self::chatgpt(model);
        }
        Self::from_environment()
    }

    /// API model preferences apply only to the same provider, endpoint and transport.
    pub fn from_environment() -> Result<Self, KnutError> {
        let config = Self::environment_default()?;
        if !config.uses_chatgpt_plan()
            && let Some(model) = crate::openai_auth::saved_api_model(&config.preference_identity())?
        {
            return config.with_model(model);
        }
        Ok(config)
    }

    /// Ignore saved model choices when explicitly restoring environment configuration.
    pub fn environment_default() -> Result<Self, KnutError> {
        Self::from_settings(|key| std::env::var(key).ok())
    }

    pub(crate) fn preference_identity(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        for part in [
            self.environment_provider.as_str(),
            self.base_url.as_str(),
            match self.transport {
                ProviderTransport::ChatCompletions => "chat-completions",
                ProviderTransport::Responses => "responses",
            },
        ] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
        format!("{:x}", digest.finalize())
    }

    pub fn chatgpt(model: impl Into<String>) -> Result<Self, KnutError> {
        let client_id = crate::openai_auth::signed_in_client()?;
        let mut config = Self::openai("", model).with_billing(BillingPath::Plan);
        config.chatgpt_client = Some(client_id);
        Ok(config)
    }

    pub fn uses_chatgpt_plan(&self) -> bool {
        self.chatgpt_client.is_some()
    }

    pub fn chatgpt_client(&self) -> Option<&str> {
        self.chatgpt_client.as_deref()
    }

    fn from_settings(get: impl Fn(&str) -> Option<String>) -> Result<Self, KnutError> {
        let provider = get("KNUT_PROVIDER").unwrap_or_else(|| "zai".to_owned());
        if !matches!(
            provider.as_str(),
            "zai" | "openai" | "openai-codex" | "chat-completions"
        ) {
            return Err(KnutError::Model(
                "KNUT_PROVIDER must be zai/openai/openai-codex/chat-completions".to_owned(),
            ));
        }
        let openai = matches!(provider.as_str(), "openai" | "openai-codex");
        let chatgpt = provider == "openai-codex";
        let chatgpt_client = if chatgpt {
            Some(crate::openai_auth::signed_in_client()?)
        } else {
            None
        };
        let api_key = if chatgpt {
            String::new()
        } else {
            get("KNUT_PROVIDER_API_KEY")
                .filter(|key| !key.trim().is_empty())
                .or_else(|| {
                    get(if openai {
                        "OPENAI_API_KEY"
                    } else {
                        "ZAI_API_KEY"
                    })
                    .filter(|key| !key.trim().is_empty())
                })
                .ok_or_else(|| {
                    KnutError::Model(format!(
                        "Set KNUT_PROVIDER_API_KEY or {}",
                        if openai {
                            "OPENAI_API_KEY"
                        } else {
                            "ZAI_API_KEY"
                        }
                    ))
                })?
        };
        let base_url = get("KNUT_PROVIDER_BASE_URL").unwrap_or_else(|| {
            if openai {
                "https://api.openai.com/v1"
            } else {
                "https://api.z.ai/api/coding/paas/v4"
            }
            .to_owned()
        });
        if chatgpt && base_url.trim_end_matches('/') != "https://api.openai.com/v1" {
            return Err(KnutError::ModelAuth(
                "ChatGPT plan usage requires https://api.openai.com/v1".to_owned(),
            ));
        }
        let model = get("KNUT_PROVIDER_MODEL").unwrap_or_else(|| {
            if openai {
                "gpt-6.1-sol"
            } else {
                "glm-5.3-flash"
            }
            .to_owned()
        });
        let mut config = Self::new(api_key, base_url, model);
        config.environment_provider = provider;
        if openai {
            config = config
                .with_transport(ProviderTransport::Responses)
                .with_billing(if chatgpt {
                    BillingPath::Plan
                } else {
                    BillingPath::Metered
                });
            config.chatgpt_client = chatgpt_client;
        }
        if let Some(seconds) = get("KNUT_PROVIDER_TIMEOUT_SECONDS") {
            let seconds = seconds
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    KnutError::Model(
                        "KNUT_PROVIDER_TIMEOUT_SECONDS must be a positive integer".to_owned(),
                    )
                })?;
            config.timeout = Duration::from_secs(seconds);
        }
        if let Some(effort) = get("KNUT_PROVIDER_REASONING_EFFORT") {
            config.explicit_reasoning_effort = true;
            config.reasoning_effort = Some(match effort.as_str() {
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                "xhigh" => ReasoningEffort::XHigh,
                "max" => ReasoningEffort::Max,
                _ => {
                    return Err(KnutError::Model(
                        "reasoning effort must be low/medium/high/xhigh/max".to_owned(),
                    ));
                }
            });
        }
        config.validate_reasoning()?;
        if let Some(tier) = get("KNUT_PROVIDER_TIER") {
            config.tier = match tier.to_ascii_lowercase().as_str() {
                "fast" => ModelTier::Fast,
                "standard" => ModelTier::Standard,
                "reasoner" => ModelTier::Reasoner,
                other => {
                    return Err(KnutError::Model(format!(
                        "KNUT_PROVIDER_TIER must be fast/standard/reasoner, got {other:?}"
                    )));
                }
            };
        }
        Ok(config)
    }

    fn validate_reasoning(&self) -> Result<(), KnutError> {
        if self.reasoning_effort.is_some() && !self.supports_reasoning_effort {
            return Err(KnutError::Model(
                "reasoning effort is not supported by this configured endpoint/model".to_owned(),
            ));
        }
        if self.model.starts_with("glm-5.3")
            && matches!(
                self.reasoning_effort,
                Some(ReasoningEffort::Medium | ReasoningEffort::XHigh)
            )
        {
            return Err(KnutError::Model(
                "GLM-5.3 reasoning effort must be low/high/max; choose an explicit native level"
                    .to_owned(),
            ));
        }
        if self.transport == ProviderTransport::Responses {
            let unsupported = match self.reasoning_effort {
                Some(ReasoningEffort::Max) => !self.model.starts_with("gpt-6"),
                Some(ReasoningEffort::XHigh) => {
                    matches!(self.model.as_str(), "gpt-5" | "gpt-5-codex" | "gpt-5.1")
                }
                _ => false,
            };
            if unsupported {
                return Err(KnutError::Model("requested reasoning effort is not supported by this model; choose a documented native level".to_owned()));
            }
        }
        Ok(())
    }

    pub fn reasoning_effort(&self) -> Option<&'static str> {
        self.reasoning_effort.map(ReasoningEffort::as_str)
    }

    /// A secret-free summary for `doctor` and logs.
    pub fn summary(&self) -> ProviderSummary {
        ProviderSummary {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            tier: self.tier,
            billing: self.billing,
            timeout: self.timeout,
            api_key_source: if self.uses_chatgpt_plan() {
                "ChatGPT sign-in"
            } else {
                "environment"
            },
            transport: self.transport,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn billing(&self) -> BillingPath {
        self.billing
    }
}

/// What may be shown: endpoint, model, tier, billing path. Never the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSummary {
    pub base_url: String,
    pub model: String,
    pub tier: ModelTier,
    pub billing: BillingPath,
    pub timeout: Duration,
    pub api_key_source: &'static str,
    pub transport: ProviderTransport,
}

/// One model adapter for Chat Completions or native Responses.
pub struct ProviderModel {
    config: ProviderConfig,
    http: reqwest::Client,
}

impl std::fmt::Debug for ProviderModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderModel")
            .field("config", &self.config)
            .finish()
    }
}

impl ProviderModel {
    pub fn new(config: ProviderConfig) -> Result<Self, KnutError> {
        config.validate_reasoning()?;
        let url = reqwest::Url::parse(&config.base_url)
            .map_err(|_| KnutError::Model("invalid provider base URL".to_owned()))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
        {
            return Err(KnutError::Model("provider URL requires HTTPS (or loopback HTTP), without credentials/query/fragment".to_owned()));
        }
        if config.model.trim().is_empty()
            || !config.uses_chatgpt_plan() && config.api_key.trim().is_empty()
        {
            return Err(KnutError::Model(
                "provider model and credential must be nonempty".to_owned(),
            ));
        }
        if config.uses_chatgpt_plan() && config.base_url != "https://api.openai.com/v1" {
            return Err(KnutError::ModelAuth(
                "ChatGPT tokens can only be sent to https://api.openai.com/v1".to_owned(),
            ));
        }
        // No redirects: a redirect is exactly how credentials would cross
        // origins.
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| KnutError::Model(format!("http client: {err}")))?;
        Ok(Self { config, http })
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    /// List account-visible models using the same credential as inference.
    pub async fn list_models(&self) -> Result<Vec<(String, String)>, KnutError> {
        let token = if let Some(client_id) = self.config.chatgpt_client() {
            crate::openai_auth::access_token(client_id).await?
        } else {
            self.config.api_key.clone()
        };
        let response = self
            .http
            .get(format!("{}/models", self.config.base_url))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| {
                KnutError::ModelUnavailable("model catalog transport failed".to_owned())
            })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|_| KnutError::Model("cannot read model catalog".to_owned()))?;
        if !status.is_success() {
            return Err(provider_error(Some(status), &body));
        }
        let body: Value = serde_json::from_str(&body)
            .map_err(|_| KnutError::Model("invalid model catalog".to_owned()))?;
        let key = if self.config.uses_chatgpt_plan() {
            "models"
        } else {
            "data"
        };
        let models = body[key]
            .as_array()
            .ok_or_else(|| KnutError::Model("model catalog has no model array".to_owned()))?;
        let mut result = Vec::new();
        for entry in models {
            if self.config.uses_chatgpt_plan() && entry["visibility"] != "list" {
                continue;
            }
            let id = entry[if self.config.uses_chatgpt_plan() {
                "slug"
            } else {
                "id"
            }]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| KnutError::Model("model catalog entry has no ID".to_owned()))?;
            result.push((
                id.to_owned(),
                entry["display_name"].as_str().unwrap_or(id).to_owned(),
            ));
        }
        Ok(result)
    }

    /// Capability report for this configured endpoint.
    ///
    /// These are the features this adapter *implements*; whether the
    /// endpoint honors them is what the live smoke test verifies.
    pub fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tools: true,
            reasoning: self.config.supports_reasoning_effort,
            continuation: true,
            usage: true,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}{DEFAULT_CHAT_PATH}", self.config.base_url)
    }

    fn validate_exchanges(&self, request: &ModelRequest) -> Result<(), KnutError> {
        let identity = self.identity();
        if request.exchanges.iter().any(|exchange| {
            exchange.identity.provider != identity.provider
                || exchange.identity.model != identity.model
        }) {
            return Err(KnutError::Model(
                "conversation belongs to another endpoint/model".to_owned(),
            ));
        }
        if self.config.transport == ProviderTransport::ChatCompletions
            && request
                .exchanges
                .iter()
                .flat_map(|exchange| &exchange.continuation.parts)
                .any(|part| part.kind != "reasoning_content")
        {
            return Err(KnutError::Model(
                "continuation belongs to another transport".to_owned(),
            ));
        }
        Ok(())
    }

    /// Build the wire request body.
    fn body(
        &self,
        request: &ModelRequest,
        stream: bool,
        continuation: Option<&Continuation>,
    ) -> Value {
        let mut messages = Vec::new();

        // Continuation parts are echoed back unmodified as ordered
        // provider state, never summarized.
        if let Some(continuation) = continuation
            && !continuation.is_empty()
        {
            messages.push(json!({
                "role": "assistant",
                "reasoning_content": continuation
                    .parts
                    .iter()
                    .map(|p| p.value.as_str())
                    .collect::<Vec<_>>()
                    .join(""),
            }));
        }

        let mut user = String::from(&request.instruction);
        if !request.input.is_null() {
            user.push_str("\n\n");
            user.push_str(&serde_json::to_string_pretty(&request.input).unwrap_or_default());
        }
        messages.push(json!({ "role": "user", "content": user }));
        for exchange in &request.exchanges {
            let mut assistant = json!({"role":"assistant", "content":exchange.content});
            if !exchange.continuation.is_empty() {
                assistant["reasoning_content"] = json!(
                    exchange
                        .continuation
                        .parts
                        .iter()
                        .map(|part| part.value.as_str())
                        .collect::<Vec<_>>()
                        .join("")
                );
            }
            if !exchange.tool_calls.is_empty() {
                assistant["tool_calls"] =
                    json!(exchange.tool_calls.iter().map(|call| json!({
                    "id":call.id,"type":"function",
                    "function":{"name":call.name,"arguments":call.arguments.to_string()},
                })).collect::<Vec<_>>());
            }
            messages.push(assistant);
            for result in &exchange.results {
                messages.push(json!({"role":"tool", "tool_call_id":result.call_id,
                    "content":result.output.to_string()}));
            }
        }

        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": stream,
        });

        if !request.tools.is_empty() {
            body["tools"] = json!(
                request
                    .tools
                    .iter()
                    .map(|tool| json!({
                        "type":"function", "function":{
                            "name":tool.function_name(), "description":tool.description,
                            "parameters":tool.input_schema,
                        },
                    }))
                    .collect::<Vec<_>>()
            );
        }

        if stream && self.config.supports_usage_in_stream {
            body["stream_options"] = json!({ "include_usage": true });
        }

        // Reasoning controls are sent only when this endpoint documents
        // them; otherwise the setting is reported as unsupported rather
        // than silently ignored.
        if self.config.supports_reasoning_effort
            && let Some(effort) = self.config.reasoning_effort
        {
            body["reasoning_effort"] = json!(effort.as_str());
            if self.config.model.starts_with("glm-5.3") {
                body["thinking"] = json!({"type": "enabled"});
            }
        }

        body
    }
}

fn provider_error(status: Option<reqwest::StatusCode>, body: &str) -> KnutError {
    let error: Value = serde_json::from_str(body).unwrap_or_default();
    let code = error["error"]["code"]
        .as_str()
        .or_else(|| error["code"].as_str())
        .unwrap_or("unknown_error");
    let code: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(100)
        .collect();
    let detail = format!(
        "provider error {code}{}",
        status
            .map(|s| format!(" (HTTP {})", s.as_u16()))
            .unwrap_or_default()
    );
    if status.is_some_and(|s| s.as_u16() == 401 || s.as_u16() == 403) {
        KnutError::ModelAuth(detail)
    } else if status.is_some_and(|s| s.as_u16() == 429)
        || matches!(
            code.as_str(),
            "rate_limit_exceeded"
                | "subscription_sharing_usage_limit_exceeded"
                | "subscription_sharing_usage_unavailable"
        )
    {
        KnutError::ModelRateLimit(detail)
    } else if status.is_some_and(|s| s.is_server_error()) || code == "server_error" {
        KnutError::ModelUnavailable(detail)
    } else {
        KnutError::Model(detail)
    }
}

#[async_trait]
impl Model for ProviderModel {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: self
                .config
                .base_url
                .split("//")
                .nth(1)
                .unwrap_or(&self.config.base_url)
                .split('/')
                .next()
                .unwrap_or("provider")
                .to_owned(),
            model: self.config.model.clone(),
            tier: self.config.tier,
        }
    }

    fn capabilities(&self) -> ModelCapabilities {
        ProviderModel::capabilities(self)
    }

    /// Continue a turn, echoing the provider's continuation state back
    /// unmodified. This is the path that preserves reasoning continuity:
    /// the default trait implementation would drop it.
    async fn continue_turn(
        &self,
        continuation: Option<&Continuation>,
        request: &ModelRequest,
    ) -> Result<ModelResponse, KnutError> {
        self.validate_exchanges(request)?;
        if self.config.transport == ProviderTransport::Responses {
            return self
                .responses_turn(request, continuation, &mut crate::BufferedSink::new())
                .await;
        }
        let started = Instant::now();
        let response = self
            .http
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&self.body(request, false, continuation))
            .send()
            .await
            .map_err(|err| KnutError::Model(format!("transport: {err}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| KnutError::Model(format!("reading response: {err}")))?;

        if !status.is_success() {
            return Err(provider_error(Some(status), &text));
        }

        let parsed: ChatCompletion = serde_json::from_str(&text)
            .map_err(|err| KnutError::Model(format!("unexpected response envelope: {err}")))?;

        parsed.into_response(
            self.identity(),
            started.elapsed(),
            &self.config.reasoning_field,
        )
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError> {
        self.validate_exchanges(request)?;
        if self.config.transport == ProviderTransport::Responses {
            return self
                .responses_turn(
                    request,
                    request
                        .exchanges
                        .last()
                        .map(|exchange| &exchange.continuation),
                    &mut crate::BufferedSink::new(),
                )
                .await;
        }
        let started = Instant::now();
        let response = self
            .http
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&self.body(request, false, None))
            .send()
            .await
            .map_err(|err| KnutError::Model(format!("transport: {err}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| KnutError::Model(format!("reading response: {err}")))?;

        if !status.is_success() {
            return Err(provider_error(Some(status), &text));
        }

        let parsed: ChatCompletion = serde_json::from_str(&text).map_err(|err| {
            // A 200 that is not the documented envelope is a protocol
            // failure, not an empty answer.
            KnutError::Model(format!("unexpected response envelope: {err}"))
        })?;

        let latency = started.elapsed();
        parsed.into_response(self.identity(), latency, &self.config.reasoning_field)
    }

    async fn stream(
        &self,
        request: &ModelRequest,
        sink: &mut (dyn ModelStreamSink + Send),
    ) -> Result<ModelResponse, KnutError> {
        self.validate_exchanges(request)?;
        if self.config.transport == ProviderTransport::Responses {
            return self
                .responses_turn(
                    request,
                    request
                        .exchanges
                        .last()
                        .map(|exchange| &exchange.continuation),
                    sink,
                )
                .await;
        }
        let started = Instant::now();
        let response = self
            .http
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&self.body(request, true, None))
            .send()
            .await
            .map_err(|err| KnutError::Model(format!("transport: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(provider_error(Some(status), &text));
        }

        let mut stream = response.bytes_stream();
        let mut lines = StreamLines::default();
        let mut accumulator = StreamAccumulator::new(self.config.reasoning_field.clone());
        let mut raw_error: Option<String> = None;

        use futures_util::StreamExt as _;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    // A transport drop mid-stream is incomplete, never a
                    // successful partial artifact.
                    let reason = format!("stream transport error: {err}");
                    sink.on_event(ModelStreamEvent::Incomplete {
                        reason: reason.clone(),
                    });
                    return Err(KnutError::Model(reason));
                }
            };

            let complete_lines = lines.push(&chunk).map_err(|reason| {
                sink.on_event(ModelStreamEvent::Incomplete {
                    reason: reason.to_owned(),
                });
                KnutError::Model(reason.to_owned())
            })?;
            for line in complete_lines {
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    accumulator.done = true;
                    continue;
                }

                let frame: StreamFrame = match serde_json::from_str(payload) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // A malformed frame means we cannot know what the
                        // turn contained: stop rather than guess.
                        let reason = format!("malformed stream frame: {err}");
                        sink.on_event(ModelStreamEvent::Incomplete {
                            reason: reason.clone(),
                        });
                        return Err(KnutError::Model(reason));
                    }
                };

                if let Some(error) = frame.error {
                    raw_error = Some(error.message);
                    continue;
                }

                for event in accumulator.absorb(&frame).inspect_err(|error| {
                    sink.on_event(ModelStreamEvent::Incomplete {
                        reason: error.to_string(),
                    });
                })? {
                    sink.on_event(event);
                }
            }
        }

        if let Some(message) = raw_error {
            // An error frame after a 200 is a failed call.
            return Err(provider_error(
                Some(reqwest::StatusCode::BAD_GATEWAY),
                &message,
            ));
        }

        if !accumulator.done && accumulator.finish_reason.is_none() {
            let reason = "stream ended without a completion frame".to_owned();
            sink.on_event(ModelStreamEvent::Incomplete {
                reason: reason.clone(),
            });
            return Err(KnutError::Model(reason));
        }

        let usage = accumulator.usage;
        let response = accumulator
            .into_response(self.identity(), started.elapsed())
            .inspect_err(|error| {
                sink.on_event(ModelStreamEvent::Incomplete {
                    reason: error.to_string(),
                });
            })?;
        sink.on_event(ModelStreamEvent::Completed { usage });
        Ok(response)
    }
}

const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;
const MAX_STREAM_TOOL_CALLS: usize = 16;

#[derive(Default)]
struct StreamLines {
    pending: Vec<u8>,
    scanned: usize,
    total: usize,
}

impl StreamLines {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, &'static str> {
        self.total = self.total.saturating_add(chunk.len());
        if self.total > MAX_STREAM_BYTES {
            return Err("provider stream exceeds the 8 MiB limit");
        }
        self.pending.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0;
        for newline in
            (self.scanned..self.pending.len()).filter(|index| self.pending[*index] == b'\n')
        {
            let line = std::str::from_utf8(&self.pending[start..newline])
                .map_err(|_| "provider stream contains invalid UTF-8")?
                .trim_end_matches('\r')
                .to_owned();
            start = newline + 1;
            lines.push(line);
        }
        self.pending.drain(..start);
        self.scanned = self.pending.len();
        Ok(lines)
    }
}

/// Accumulates OpenAI-compatible stream frames into a response.
struct StreamAccumulator {
    reasoning_field: String,
    content: String,
    reasoning: Vec<String>,
    calls: Vec<PartialCall>,
    usage: Usage,
    finish_reason: Option<String>,
    done: bool,
}

struct PartialCall {
    id: String,
    name: String,
    arguments: String,
    emitted_end: bool,
}

impl StreamAccumulator {
    fn new(reasoning_field: String) -> Self {
        Self {
            reasoning_field,
            content: String::new(),
            reasoning: Vec::new(),
            calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: None,
            done: false,
        }
    }

    fn absorb(&mut self, frame: &StreamFrame) -> Result<Vec<ModelStreamEvent>, KnutError> {
        let mut events = Vec::new();

        if let Some(usage) = &frame.usage {
            // Report exactly what arrived; absent fields stay unknown.
            self.usage = Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            };
        }

        for choice in &frame.choices {
            if let Some(reason) = &choice.finish_reason {
                self.finish_reason = Some(reason.clone());
                // Close any call still open so its arguments are only
                // published once complete.
                for call in &mut self.calls {
                    if !call.emitted_end {
                        call.emitted_end = true;
                        events.push(ModelStreamEvent::ToolCallEnded {
                            id: call.id.clone(),
                        });
                    }
                }
            }

            let Some(delta) = &choice.delta else {
                continue;
            };

            if let Some(reasoning) = delta.reasoning(&self.reasoning_field) {
                self.reasoning.push(reasoning.to_owned());
                events.push(ModelStreamEvent::ReasoningDelta {
                    text: reasoning.to_owned(),
                });
            }

            if let Some(content) = &delta.content
                && !content.is_empty()
            {
                self.content.push_str(content);
                events.push(ModelStreamEvent::TextDelta {
                    text: content.clone(),
                });
            }

            for call in &delta.tool_calls {
                let index = call.index.unwrap_or(0);
                if index >= MAX_STREAM_TOOL_CALLS {
                    return Err(KnutError::Model(
                        "provider returned an out-of-range tool call index".to_owned(),
                    ));
                }
                while self.calls.len() <= index {
                    self.calls.push(PartialCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                        emitted_end: false,
                    });
                }
                let slot = &mut self.calls[index];
                if let Some(id) = &call.id {
                    slot.id = id.clone();
                }
                if let Some(function) = &call.function {
                    if let Some(name) = &function.name {
                        slot.name = name.clone();
                        events.push(ModelStreamEvent::ToolCallStarted {
                            id: slot.id.clone(),
                            name: name.clone(),
                        });
                    }
                    if let Some(arguments) = &function.arguments
                        && !arguments.is_empty()
                    {
                        slot.arguments.push_str(arguments);
                        events.push(ModelStreamEvent::ToolCallArgumentsDelta {
                            id: slot.id.clone(),
                            delta: arguments.clone(),
                        });
                    }
                }
            }
        }

        Ok(events)
    }

    fn into_response(
        self,
        identity: ModelIdentity,
        latency: Duration,
    ) -> Result<ModelResponse, KnutError> {
        if self.finish_reason.as_deref() == Some("length") {
            return Err(KnutError::Model(
                "provider stopped at its output limit; the answer is truncated".to_owned(),
            ));
        }
        let mut tool_calls = Vec::new();
        for call in self.calls {
            if call.name.is_empty() {
                continue;
            }
            // Arguments must be complete and parseable: a partial JSON
            // fragment is never executed.
            let arguments: Value = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.arguments).map_err(|err| {
                    KnutError::Model(format!(
                        "tool call {} had incomplete arguments: {err}",
                        call.name
                    ))
                })?
            };
            tool_calls.push(ToolCall {
                id: call.id,
                name: call.name,
                arguments,
            });
        }

        let continuation = if self.reasoning.is_empty() {
            Continuation::new()
        } else {
            Continuation {
                parts: self
                    .reasoning
                    .into_iter()
                    .map(|value| ContinuationPart {
                        kind: "reasoning_content".to_owned(),
                        value,
                    })
                    .collect(),
            }
        };

        Ok(ModelResponse {
            content: self.content,
            identity,
            usage: self.usage,
            latency,
            tool_calls,
            continuation,
        })
    }
}

// --- wire types ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StreamFrame {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    /// GLM's documented preserved-thinking field.
    #[serde(default)]
    reasoning_content: Option<String>,
    /// Some compatible endpoints use `reasoning` instead.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

impl StreamDelta {
    fn reasoning(&self, field: &str) -> Option<&str> {
        let (preferred, fallback) = if field == "reasoning" {
            (&self.reasoning, &self.reasoning_content)
        } else {
            (&self.reasoning_content, &self.reasoning)
        };
        preferred
            .as_deref()
            .or(fallback.as_deref())
            .filter(|text| !text.is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct WireToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunction>,
}

#[derive(Debug, Deserialize)]
struct WireFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
}

/// The non-streaming envelope.
#[derive(Debug, Deserialize)]
struct ChatCompletion {
    model: Option<String>,
    choices: Vec<CompletionChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct CompletionChoice {
    message: CompletionMessage,
    /// Kept for traces: a `length` finish means the answer was cut off.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CompletionMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

impl ChatCompletion {
    fn into_response(
        self,
        mut identity: ModelIdentity,
        latency: Duration,
        reasoning_field: &str,
    ) -> Result<ModelResponse, KnutError> {
        // Record what actually answered, not what we asked for.
        if let Some(model) = &self.model {
            identity.model = model.clone();
        }

        let choice = self
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| KnutError::Model("provider returned no choices".to_owned()))?;

        let mut tool_calls = Vec::new();
        for call in choice.message.tool_calls {
            let name = call
                .function
                .as_ref()
                .and_then(|f| f.name.clone())
                .ok_or_else(|| KnutError::Model("tool call without a name".to_owned()))?;
            let raw = call
                .function
                .as_ref()
                .and_then(|f| f.arguments.clone())
                .unwrap_or_default();
            let arguments: Value = if raw.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&raw).map_err(|err| {
                    KnutError::Model(format!("tool call {name} had invalid arguments: {err}"))
                })?
            };
            tool_calls.push(ToolCall {
                id: call.id.unwrap_or_else(|| name.clone()),
                name,
                arguments,
            });
        }

        // A truncated answer is not a complete artifact; the caller must
        // see that rather than treating a cut-off answer as final.
        if choice.finish_reason.as_deref() == Some("length") {
            return Err(KnutError::Model(
                "provider stopped at its output limit; the answer is truncated".to_owned(),
            ));
        }

        let reasoning = if reasoning_field == "reasoning" {
            choice
                .message
                .reasoning
                .or(choice.message.reasoning_content)
        } else {
            choice
                .message
                .reasoning_content
                .or(choice.message.reasoning)
        }
        .filter(|text| !text.is_empty());
        let continuation = match reasoning {
            Some(value) => Continuation {
                parts: vec![ContinuationPart {
                    kind: "reasoning_content".to_owned(),
                    value,
                }],
            },
            None => Continuation::new(),
        };

        Ok(ModelResponse {
            content: choice.message.content.unwrap_or_default(),
            identity,
            usage: match self.usage {
                Some(usage) => Usage {
                    input_tokens: usage.prompt_tokens,
                    output_tokens: usage.completion_tokens,
                },
                None => Usage::default(),
            },
            latency,
            tool_calls,
            continuation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExpectedArtifact;

    /// A minimal HTTP/1.1 server for protocol fixtures.
    ///
    /// CI stays fully offline: the adapter is exercised against a real
    /// socket speaking the documented wire format, not against a mock
    /// object that could disagree with it.
    pub(super) struct FixtureServer {
        addr: std::net::SocketAddr,
        seen: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
        heads: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FixtureServer {
        pub(super) async fn start(
            responses: Vec<(u16, String)>,
        ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
            Self::start_chunked(responses, usize::MAX).await
        }

        pub(super) async fn start_chunked(
            responses: Vec<(u16, String)>,
            chunk_size: usize,
        ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind fixture server");
            let addr = listener.local_addr().expect("addr");
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen_for_task = std::sync::Arc::clone(&seen);
            let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let heads_for_task = std::sync::Arc::clone(&heads);
            let responses = std::sync::Arc::new(std::sync::Mutex::new(responses));

            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = std::sync::Arc::clone(&seen_for_task);
                    let heads = std::sync::Arc::clone(&heads_for_task);
                    let responses = std::sync::Arc::clone(&responses);
                    tokio::spawn(async move {
                        let mut buffer = vec![0u8; 65536];
                        let mut read = 0usize;
                        // Read until the headers and the declared body are in.
                        loop {
                            let Ok(n) = socket.read(&mut buffer[read..]).await else {
                                return;
                            };
                            if n == 0 {
                                break;
                            }
                            read += n;
                            let text = String::from_utf8_lossy(&buffer[..read]).to_string();
                            if let Some(split) = text.find("\r\n\r\n") {
                                let head = &text[..split];
                                let content_length = head
                                    .lines()
                                    .find_map(|line| {
                                        let (key, value) = line.split_once(':')?;
                                        if key.eq_ignore_ascii_case("content-length") {
                                            value.trim().parse::<usize>().ok()
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or(0);
                                let body = &text[split + 4..];
                                if body.len() >= content_length {
                                    heads.lock().unwrap().push(head.to_owned());
                                    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
                                        seen.lock().unwrap().push(parsed);
                                    }
                                    let (status, payload) = responses.lock().unwrap().remove(0);
                                    let response = format!(
                                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                        payload.len()
                                    );
                                    for chunk in response.as_bytes().chunks(chunk_size) {
                                        if socket.write_all(chunk).await.is_err() {
                                            return;
                                        }
                                        tokio::task::yield_now().await;
                                    }
                                    let _ = socket.flush().await;
                                    return;
                                }
                            }
                        }
                    });
                }
            });

            (
                Self {
                    addr,
                    seen: std::sync::Arc::clone(&seen),
                    heads,
                },
                seen,
            )
        }

        pub(super) fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        pub(super) fn requests(&self) -> Vec<Value> {
            self.seen.lock().unwrap().clone()
        }

        pub(super) fn headers(&self) -> Vec<String> {
            self.heads.lock().unwrap().clone()
        }
    }

    /// One SSE frame.
    pub(super) fn sse(payload: &str) -> String {
        format!("data: {payload}\n\n")
    }

    /// The documented GLM turn: reasoning, then a tool call, then a final
    /// artifact, with usage in the terminal frame.
    fn reasoning_tool_then_artifact_stream() -> String {
        let mut body = String::new();
        body.push_str(&sse(
            r#"{"id":"1","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"I should read the file"}}]}"#,
        ));
        // Arguments arrive fragmented across frames.
        body.push_str(&sse(
            r#"{"id":"1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
        ));
        body.push_str(&sse(
            r#"{"id":"1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/lib.rs\"}"}}]}}]}"#,
        ));
        body.push_str(&sse(
            r#"{"id":"1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ));
        body.push_str(&sse(
            r#"{"id":"1","choices":[{"index":0,"delta":{"role":"assistant","content":"the fix is ready"}}]}"#,
        ));
        body.push_str(&sse(
            r#"{"id":"1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":120,"completion_tokens":45}}"#,
        ));
        body.push_str("data: [DONE]\n\n");
        body
    }

    #[tokio::test]
    async fn live_http_fixture_completes_reasoning_tool_call_and_artifact() {
        let (server, _) =
            FixtureServer::start(vec![(200, reasoning_tool_then_artifact_stream())]).await;

        let config = ProviderConfig::new("test-key", server.base_url(), "glm-5.3-flash");
        let model = ProviderModel::new(config).unwrap();

        let mut sink = crate::BufferedSink::new();
        let response = model
            .stream(
                &ModelRequest::new("fix the bug", ExpectedArtifact::Text)
                    .with_input(json!({ "file": "src/lib.rs" })),
                &mut sink,
            )
            .await
            .expect("streamed turn");

        // The artifact is the assistant text, not the reasoning.
        assert_eq!(response.content, "the fix is ready");
        // Usage came from the terminal frame and is real.
        assert_eq!(response.usage, Usage::known(120, 45));
        // Continuation is the provider's reasoning, preserved verbatim.
        assert_eq!(response.continuation.parts.len(), 1);
        assert_eq!(
            response.continuation.parts[0].value,
            "I should read the file"
        );
        // The tool call's arguments are complete and parsed.
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_9");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(
            response.tool_calls[0].arguments,
            json!({ "path": "src/lib.rs" })
        );

        // The auth header was sent, and the model asked for usage.
        let sent = server.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["stream"], json!(true));
        assert_eq!(sent[0]["stream_options"]["include_usage"], json!(true));
        assert_eq!(sent[0]["model"], json!("glm-5.3-flash"));
    }

    #[tokio::test]
    async fn continuation_is_echoed_on_the_next_request() {
        let (server, _) = FixtureServer::start(vec![
            (200, reasoning_tool_then_artifact_stream()),
            // `continue_turn` uses the non-streaming transport.
            (
                200,
                r#"{"model":"glm-5.3-flash","choices":[{"message":{"content":"continuing"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_owned(),
            ),
        ])
        .await;

        let model =
            ProviderModel::new(ProviderConfig::new("k", server.base_url(), "glm-5.3-flash"))
                .unwrap();

        let mut sink = crate::BufferedSink::new();
        let first = model
            .stream(&ModelRequest::new("one", ExpectedArtifact::Text), &mut sink)
            .await
            .unwrap();

        // Second turn carries the continuation verbatim.
        let second = model
            .continue_turn(
                Some(&first.continuation),
                &ModelRequest::new("two", ExpectedArtifact::Text),
            )
            .await
            .unwrap();
        assert_eq!(second.content, "continuing");

        let sent = server.requests();
        assert_eq!(sent.len(), 2);
        let messages = sent[1]["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], json!("assistant"));
        assert_eq!(
            messages[0]["reasoning_content"],
            json!("I should read the file")
        );
        assert_eq!(messages[1]["role"], json!("user"));
    }

    #[tokio::test]
    async fn fragmented_frames_across_tcp_chunks_reassemble() {
        let (server, _) =
            FixtureServer::start(vec![(200, reasoning_tool_then_artifact_stream())]).await;

        let model =
            ProviderModel::new(ProviderConfig::new("k", server.base_url(), "glm-5.3-flash"))
                .unwrap();
        let mut sink = crate::BufferedSink::new();
        let response = model
            .stream(&ModelRequest::new("x", ExpectedArtifact::Text), &mut sink)
            .await
            .unwrap();
        assert_eq!(response.content, "the fix is ready");
    }

    #[tokio::test]
    async fn rate_limit_and_server_errors_are_typed() {
        let (server, _) = FixtureServer::start(vec![
            (429, r#"{"error":{"message":"rate limited"}}"#.to_owned()),
            (
                500,
                r#"{"error":{"message":"upstream exploded"}}"#.to_owned(),
            ),
            (401, r#"{"error":{"message":"invalid api key"}}"#.to_owned()),
        ])
        .await;

        let model =
            ProviderModel::new(ProviderConfig::new("k", server.base_url(), "glm-5.3-flash"))
                .unwrap();

        let err = model
            .complete(&ModelRequest::new("x", ExpectedArtifact::Text))
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::ModelRateLimit(_)), "got {err:?}");

        let err = model
            .complete(&ModelRequest::new("x", ExpectedArtifact::Text))
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::ModelUnavailable(_)), "got {err:?}");

        let err = model
            .complete(&ModelRequest::new("x", ExpectedArtifact::Text))
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::ModelAuth(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn unexpected_envelope_is_a_protocol_failure_not_an_empty_answer() {
        let (server, _) =
            FixtureServer::start(vec![(200, r#"{"not":"a completion"}"#.to_owned())]).await;

        let model =
            ProviderModel::new(ProviderConfig::new("k", server.base_url(), "glm-5.3-flash"))
                .unwrap();

        let err = model
            .complete(&ModelRequest::new("x", ExpectedArtifact::Text))
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::Model(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn truncated_streams_cannot_emit_a_successful_completion() {
        let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"half an answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
        let (server, _) = FixtureServer::start(vec![(200, payload.into())]).await;
        let model =
            ProviderModel::new(ProviderConfig::new("k", server.base_url(), "glm-5.3-flash"))
                .unwrap();
        let mut sink = crate::BufferedSink::new();
        let error = model
            .stream(&ModelRequest::new("x", ExpectedArtifact::Text), &mut sink)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("truncated"));
        assert!(
            sink.events()
                .iter()
                .any(|event| matches!(event, ModelStreamEvent::Incomplete { .. }))
        );
        assert!(
            !sink
                .events()
                .iter()
                .any(|event| matches!(event, ModelStreamEvent::Completed { .. }))
        );
    }

    #[tokio::test]
    async fn truncated_answer_is_reported_not_silently_accepted() {
        let (server, _) = FixtureServer::start(vec![(
            200,
            r#"{"model":"glm-5.3-flash","choices":[{"message":{"content":"half an ans"},"finish_reason":"length"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#.to_owned(),
        )])
        .await;

        let model =
            ProviderModel::new(ProviderConfig::new("k", server.base_url(), "glm-5.3-flash"))
                .unwrap();

        let err = model
            .complete(&ModelRequest::new("x", ExpectedArtifact::Text))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("truncated"), "got {err}");
    }

    #[tokio::test]
    async fn stream_that_ends_without_completion_is_an_error() {
        let (server, _) = FixtureServer::start(vec![(
            200,
            sse(r#"{"id":"1","choices":[{"index":0,"delta":{"content":"partial"}}]}"#),
        )])
        .await;

        let model =
            ProviderModel::new(ProviderConfig::new("k", server.base_url(), "glm-5.3-flash"))
                .unwrap();

        let mut sink = crate::BufferedSink::new();
        let err = model
            .stream(&ModelRequest::new("x", ExpectedArtifact::Text), &mut sink)
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::Model(_)), "got {err:?}");
        // The sink saw the incomplete marker, so a UI can stop cleanly.
        assert!(
            sink.events()
                .iter()
                .any(|e| matches!(e, ModelStreamEvent::Incomplete { .. }))
        );
    }

    #[tokio::test]
    async fn malformed_frame_stops_the_turn_instead_of_guessing() {
        let (server, _) = FixtureServer::start(vec![(200, sse("{not json"))]).await;

        let model =
            ProviderModel::new(ProviderConfig::new("k", server.base_url(), "glm-5.3-flash"))
                .unwrap();

        let mut sink = crate::BufferedSink::new();
        let err = model
            .stream(&ModelRequest::new("x", ExpectedArtifact::Text), &mut sink)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("malformed"), "got {err}");
    }

    /// Live smoke test: opt-in only, so ordinary CI never needs a key or
    /// a network. Verifies the real GLM endpoint against this adapter.
    #[tokio::test]
    async fn live_glm_smoke_is_opt_in() {
        let Ok(api_key) = std::env::var("ZAI_API_KEY") else {
            eprintln!("skipping: ZAI_API_KEY not set");
            return;
        };
        if std::env::var("KNUT_LIVE_SMOKE").as_deref() != Ok("1") {
            eprintln!("skipping: set KNUT_LIVE_SMOKE=1 to run the live smoke test");
            return;
        }

        let model = ProviderModel::new(ProviderConfig::glm_coding(api_key)).unwrap();
        assert!(model.capabilities().streaming);

        let mut sink = crate::BufferedSink::new();
        let response = model
            .stream(
                &ModelRequest::new(
                    "Reply with exactly the word pong and nothing else.",
                    ExpectedArtifact::Text,
                ),
                &mut sink,
            )
            .await
            .expect("live GLM call");

        assert!(
            response.content.to_lowercase().contains("pong"),
            "unexpected content: {:?}",
            response.content
        );
        // Real usage arrives from the provider.
        assert!(response.usage.is_known());
        // Reasoning continuity is preserved by the documented field.
        eprintln!(
            "live GLM ok: model={} usage={:?} reasoning_parts={} content={:?}",
            response.identity.model,
            response.usage,
            response.continuation.parts.len(),
            response.content
        );
    }

    fn identity() -> ModelIdentity {
        ModelIdentity {
            provider: "api.z.ai".to_owned(),
            model: "glm-5.3-flash".to_owned(),
            tier: ModelTier::Reasoner,
        }
    }

    #[test]
    fn model_changes_recompute_capabilities_and_preserve_explicit_effort() {
        let changed = ProviderConfig::glm_coding("secret")
            .with_model("glm-5.3")
            .unwrap();
        assert_eq!(changed.reasoning_effort(), Some("max"));
        let ordinary = changed.with_model("ordinary-chat-model").unwrap();
        assert!(!ordinary.supports_reasoning_effort);
        assert_eq!(ordinary.reasoning_effort(), None);
        assert_eq!(ordinary.api_key, "secret");
        assert_eq!(ordinary.billing, BillingPath::Plan);

        let explicit = ProviderConfig::openai("secret", "gpt-6.1-sol")
            .with_reasoning_effort(ReasoningEffort::Max);
        assert!(explicit.clone().with_model("gpt-5").is_err());
        assert!(explicit.clone().with_model("gpt-4.1").is_err());
        assert_eq!(
            explicit.with_model("gpt-6-sol").unwrap().reasoning_effort(),
            Some("max")
        );
        let reasoning = ProviderConfig::openai("secret", "gpt-4.1")
            .with_model("gpt-6.1-sol")
            .unwrap();
        assert!(reasoning.supports_reasoning_effort);
        assert!(
            ProviderConfig::glm_coding("secret")
                .with_model("\n")
                .is_err()
        );
    }

    #[test]
    fn model_preference_identity_tracks_provider_endpoint_and_transport_only() {
        let config = ProviderConfig::openai("first-secret", "first-model");
        let identity = config.preference_identity();
        assert_eq!(identity.len(), 64);
        assert!(identity.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            identity,
            ProviderConfig::openai("different-secret", "different-model").preference_identity()
        );
        assert_ne!(
            identity,
            config
                .clone()
                .with_transport(ProviderTransport::ChatCompletions)
                .preference_identity()
        );
        let mut other = config.clone();
        other.base_url = "https://api.openai.com/other/v1".to_owned();
        assert_ne!(identity, other.preference_identity());
        other = config;
        other.environment_provider = "chat-completions".to_owned();
        assert_ne!(identity, other.preference_identity());

        let env = ProviderConfig::from_settings(|key| match key {
            "KNUT_PROVIDER" => Some("openai".to_owned()),
            "OPENAI_API_KEY" => Some("secret".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(identity, env.preference_identity());
    }

    #[test]
    fn explicit_provider_selection_never_borrows_another_providers_key() {
        let settings = |values: &[(&str, &str)]| -> Result<ProviderConfig, KnutError> {
            ProviderConfig::from_settings(|key| {
                values
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            })
        };
        let config = settings(&[
            ("KNUT_PROVIDER", "openai"),
            ("OPENAI_API_KEY", "openai-secret"),
            ("ZAI_API_KEY", "zai-secret"),
        ])
        .unwrap();
        assert_eq!(config.api_key, "openai-secret");
        assert_eq!(config.transport, ProviderTransport::Responses);
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.billing, BillingPath::Metered);
        assert!(settings(&[("KNUT_PROVIDER", "openai"), ("ZAI_API_KEY", "zai-secret")]).is_err());
        let zai = settings(&[
            ("ZAI_API_KEY", "zai-secret"),
            ("OPENAI_API_KEY", "openai-secret"),
        ])
        .unwrap();
        assert_eq!(zai.api_key, "zai-secret");
        assert_eq!(zai.transport, ProviderTransport::ChatCompletions);
        assert!(
            settings(&[
                ("KNUT_PROVIDER", "unknown"),
                ("KNUT_PROVIDER_API_KEY", "secret")
            ])
            .is_err()
        );
        assert!(
            ProviderModel::new(ProviderConfig::new(
                "key",
                "https://user:secret@example.com/v1",
                "model"
            ))
            .is_err()
        );
        assert!(
            ProviderModel::new(ProviderConfig::new(
                "key",
                "http://remote.invalid/v1",
                "model"
            ))
            .is_err()
        );
    }

    #[test]
    fn provider_errors_do_not_echo_credentials_or_prompt_text() {
        let error = provider_error(
            Some(reqwest::StatusCode::UNAUTHORIZED),
            r#"{"error":{"code":"invalid_api_key","message":"Incorrect API key: secret-key; prompt: private"}}"#,
        );
        let message = error.to_string();
        assert!(message.contains("invalid_api_key"));
        assert!(!message.contains("secret-key"));
        assert!(!message.contains("private"));
        assert!(matches!(error, KnutError::ModelAuth(_)));
    }

    #[test]
    fn config_debug_never_reveals_the_key() {
        let config = ProviderConfig::glm_coding("super-secret-key");
        let debug = format!("{config:?}");
        assert!(!debug.contains("super-secret-key"));
        assert!(debug.contains("[redacted]"));

        let summary = config.summary();
        assert!(!format!("{summary:?}").contains("super-secret-key"));
        assert_eq!(summary.model, "glm-5.3-flash");
        assert_eq!(summary.billing, BillingPath::Plan);
    }

    #[test]
    fn coding_and_metered_endpoints_are_distinct_billing_paths() {
        let plan = ProviderConfig::glm_coding("k");
        let metered = ProviderConfig::glm_metered("k");
        assert_eq!(plan.billing(), BillingPath::Plan);
        assert_eq!(metered.billing(), BillingPath::Metered);
        assert_ne!(plan.base_url(), metered.base_url());
    }

    #[test]
    fn missing_usage_on_the_wire_stays_unknown() {
        let frame: StreamFrame =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).unwrap();
        assert!(frame.usage.is_none());

        let accumulator = StreamAccumulator::new("reasoning_content".to_owned());
        let usage = accumulator.usage;
        assert!(!usage.is_known());
    }

    #[test]
    fn byte_fragmented_stream_lines_preserve_unicode_and_reject_invalid_encoding() {
        let text = "data: hé界🙂\r\n\ndata: next\n";
        let mut lines = StreamLines::default();
        let mut complete = Vec::new();
        for byte in text.as_bytes() {
            complete.extend(lines.push(&[*byte]).unwrap());
        }
        assert_eq!(complete, ["data: hé界🙂", "", "data: next"]);
        assert!(lines.pending.is_empty());
        assert!(StreamLines::default().push(b"data: \xff\n").is_err());
    }

    #[test]
    fn provider_streams_are_bounded_and_tool_indexes_do_not_allocate_arbitrarily() {
        let mut lines = StreamLines::default();
        assert!(
            lines
                .push(&vec![b'x'; MAX_STREAM_BYTES])
                .unwrap()
                .is_empty()
        );
        assert!(lines.push(b"x").is_err());
        let frame: StreamFrame = serde_json::from_value(json!({
            "choices":[{"delta":{"tool_calls":[{"index":usize::MAX,"id":"c","function":{"name":"write","arguments":"{}"}}]}}]
        })).unwrap();
        let mut accumulator = StreamAccumulator::new("reasoning_content".to_owned());
        assert!(accumulator.absorb(&frame).is_err());
        assert!(accumulator.calls.is_empty());
    }

    #[test]
    fn configured_reasoning_field_selects_the_provider_continuation() {
        let frame: StreamFrame = serde_json::from_value(
            json!({"choices":[{"delta":{"reasoning_content":"fallback","reasoning":"preferred"}}]}),
        )
        .unwrap();
        let mut accumulator = StreamAccumulator::new("reasoning".into());
        accumulator.absorb(&frame).unwrap();
        assert_eq!(accumulator.reasoning, ["preferred"]);
        let completion: ChatCompletion = serde_json::from_value(json!({"choices":[{"message":{"content":"answer","reasoning_content":"fallback","reasoning":"preferred"}}]})).unwrap();
        let response = completion
            .into_response(identity(), Duration::ZERO, "reasoning")
            .unwrap();
        assert_eq!(response.continuation.parts[0].value, "preferred");
    }

    #[test]
    fn reasoning_content_precedes_content_and_is_preserved() {
        let mut accumulator = StreamAccumulator::new("reasoning_content".to_owned());

        let frames = [
            r#"{"choices":[{"delta":{"reasoning_content":"think "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"hard"}}]}"#,
            r#"{"choices":[{"delta":{"content":"answer"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":22}}"#,
        ];

        let mut events = Vec::new();
        for raw in frames {
            let frame: StreamFrame = serde_json::from_str(raw).unwrap();
            events.extend(accumulator.absorb(&frame).unwrap());
        }

        // Reasoning arrived as reasoning deltas, not as text deltas.
        assert!(matches!(
            &events[0],
            ModelStreamEvent::ReasoningDelta { text } if text == "think "
        ));
        assert!(!events.iter().any(|e| matches!(
            e,
            ModelStreamEvent::TextDelta { text } if text == "think "
        )));

        let response = accumulator
            .into_response(identity(), Duration::ZERO)
            .unwrap();
        assert_eq!(response.content, "answer");
        assert_eq!(response.usage, Usage::known(11, 22));
        // Preserved verbatim, as ordered provider parts.
        assert_eq!(response.continuation.parts.len(), 2);
        assert_eq!(response.continuation.parts[0].value, "think ");
        assert_eq!(response.continuation.parts[1].value, "hard");
    }

    #[test]
    fn fragmented_tool_arguments_are_only_published_complete() {
        let mut accumulator = StreamAccumulator::new("reasoning_content".to_owned());

        let frames = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];

        let mut events = Vec::new();
        for raw in frames {
            let frame: StreamFrame = serde_json::from_str(raw).unwrap();
            events.extend(accumulator.absorb(&frame).unwrap());
        }

        // Start, argument deltas, then end only at finish.
        assert!(matches!(
            &events[0],
            ModelStreamEvent::ToolCallStarted { name, .. } if name == "read_file"
        ));
        assert!(matches!(
            events.last(),
            Some(ModelStreamEvent::ToolCallEnded { .. })
        ));

        let response = accumulator
            .into_response(identity(), Duration::ZERO)
            .unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].arguments, json!({ "path": "a.rs" }));
    }

    #[test]
    fn incomplete_tool_arguments_never_become_a_call() {
        let mut accumulator = StreamAccumulator::new("reasoning_content".to_owned());
        // Stream stopped mid-arguments with no finish frame.
        let frame: StreamFrame = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"write","arguments":"{\"path\":"}}]}}]}"#,
        )
        .unwrap();
        accumulator.absorb(&frame).unwrap();

        assert!(
            accumulator
                .into_response(identity(), Duration::ZERO)
                .is_err()
        );
    }

    #[test]
    fn continuation_round_trips_into_the_next_request() {
        let config = ProviderConfig::glm_coding("k");
        let model = ProviderModel::new(config).unwrap();

        let continuation = Continuation {
            parts: vec![
                ContinuationPart {
                    kind: "reasoning_content".to_owned(),
                    value: "first".to_owned(),
                },
                ContinuationPart {
                    kind: "reasoning_content".to_owned(),
                    value: "second".to_owned(),
                },
            ],
        };

        let body = model.body(
            &ModelRequest::new("continue", ExpectedArtifact::Text),
            false,
            Some(&continuation),
        );

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        // Verbatim, in order, unmodified.
        assert_eq!(messages[0]["reasoning_content"], "firstsecond");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn stream_requests_ask_for_usage_when_supported() {
        let model = ProviderModel::new(ProviderConfig::glm_coding("k")).unwrap();
        let body = model.body(&ModelRequest::new("hi", ExpectedArtifact::Text), true, None);
        assert_eq!(body["stream_options"]["include_usage"], true);

        let model = ProviderModel::new(ProviderConfig::glm_coding("k").with_usage_in_stream(false))
            .unwrap();
        let body = model.body(&ModelRequest::new("hi", ExpectedArtifact::Text), true, None);
        // Never ask for a control the endpoint does not document.
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn glm_native_reasoning_levels_are_sent_and_unsupported_levels_refused() {
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            let model =
                ProviderModel::new(ProviderConfig::glm_coding("k").with_reasoning_effort(effort))
                    .unwrap();
            let body = model.body(&ModelRequest::new("x", ExpectedArtifact::Text), false, None);
            assert_eq!(body["reasoning_effort"], effort.as_str());
            assert_eq!(body["thinking"]["type"], "enabled");
        }
        assert!(
            ProviderModel::new(
                ProviderConfig::glm_coding("k").with_reasoning_effort(ReasoningEffort::Medium)
            )
            .is_err()
        );
        assert!(
            ProviderModel::new(
                ProviderConfig::new("k", "http://localhost:1234", "unknown")
                    .with_reasoning_effort(ReasoningEffort::High)
            )
            .is_err()
        );
    }

    #[test]
    fn capability_report_is_honest_about_reasoning_controls() {
        let model = ProviderModel::new(ProviderConfig::glm_coding("k")).unwrap();
        let caps = model.capabilities();
        assert!(caps.streaming);
        assert!(caps.tools);
        assert!(caps.continuation);
        assert!(caps.usage);
        assert!(caps.reasoning);
    }

    #[test]
    fn structured_input_is_serialized_into_the_user_message() {
        let model = ProviderModel::new(ProviderConfig::glm_coding("k")).unwrap();
        let body = model.body(
            &ModelRequest::new("summarize", ExpectedArtifact::Json)
                .with_input(json!({ "note": { "content": "alpha" } })),
            false,
            None,
        );
        // No continuation: the user turn is the only message.
        let user = body["messages"][0]["content"].as_str().unwrap();
        assert!(user.contains("summarize"));
        assert!(user.contains("alpha"));
    }

    pub(super) fn native_fixture_tool() -> crate::ToolMetadata {
        crate::ToolMetadata {
            id: "external/read".to_owned(),
            tool_version: "1".to_owned(),
            capability: "documents".to_owned(),
            description: "Read the report".to_owned(),
            input_schema: json!({"type":"object"}),
            side_effect: crate::SideEffect::ReadOnly,
        }
    }

    #[tokio::test]
    async fn native_chat_tools_preserve_call_ids_results_and_reasoning() {
        let tool = native_fixture_tool();
        let first = json!({"choices":[{"message":{"role":"assistant", "content":"Inspecting",
            "reasoning_content":"opaque reasoning", "tool_calls":[{"id":"report-call", "type":"function",
                "function":{"name":tool.function_name(), "arguments":"{}"}}]}, "finish_reason":"tool_calls"}]}).to_string();
        let final_response = json!({"choices":[{"message":{"role":"assistant", "content":"The report is ready"}, "finish_reason":"stop"}]}).to_string();
        let (server, _) = FixtureServer::start(vec![(200, first), (200, final_response)]).await;
        let model = ProviderModel::new(ProviderConfig::new(
            "fixture",
            server.base_url(),
            "glm-5.3-flash",
        ))
        .unwrap();
        let request = ModelRequest::new("Summarize the report", ExpectedArtifact::Text)
            .with_tools(vec![tool.clone()]);
        let response = model.complete(&request).await.unwrap();
        let request = request.with_exchanges(vec![crate::ModelExchange {
            identity: response.identity.clone(),
            content: response.content,
            tool_calls: response.tool_calls,
            continuation: response.continuation,
            results: vec![crate::ToolResult {
                call_id: "report-call".to_owned(),
                output: json!({"report":"ready"}),
            }],
        }]);
        assert_eq!(
            model.complete(&request).await.unwrap().content,
            "The report is ready"
        );
        let sent = server.requests();
        assert_eq!(
            sent[0]["tools"][0]["function"]["name"],
            tool.function_name()
        );
        assert_eq!(sent[1]["messages"][0]["role"], "user");
        assert_eq!(
            sent[1]["messages"][1]["reasoning_content"],
            "opaque reasoning"
        );
        assert_eq!(sent[1]["messages"][1]["tool_calls"][0]["id"], "report-call");
        assert_eq!(sent[1]["messages"][2]["tool_call_id"], "report-call");
        assert_eq!(
            serde_json::from_str::<Value>(sent[1]["messages"][2]["content"].as_str().unwrap())
                .unwrap(),
            json!({"report":"ready"})
        );
    }

    #[tokio::test]
    async fn native_exchanges_cannot_be_sent_to_another_model() {
        let model = ProviderModel::new(ProviderConfig::glm_coding("fixture")).unwrap();
        let request = ModelRequest::new("task", ExpectedArtifact::Text).with_exchanges(vec![
            crate::ModelExchange {
                identity: ModelIdentity {
                    provider: "foreign".to_owned(),
                    model: "foreign".to_owned(),
                    tier: ModelTier::Reasoner,
                },
                content: "answer".to_owned(),
                tool_calls: Vec::new(),
                continuation: Continuation::new(),
                results: Vec::new(),
            },
        ]);
        assert!(
            model
                .complete(&request)
                .await
                .unwrap_err()
                .to_string()
                .contains("another endpoint/model")
        );
    }
}
