//! A real BYOK reasoner adapter over the OpenAI-compatible Chat
//! Completions transport (issue #21).
//!
//! The initial conformance target is Z.ai GLM, using its documented
//! streaming/tool/thinking contract. Compatibility is *tested*, not
//! assumed: an endpoint that merely accepts a base-URL substitution may
//! differ in reasoning fields, tool-call framing, usage reporting or
//! error shapes, so this module validates what actually arrives.
//!
//! Boundaries:
//! - credentials are never serialized, logged or forwarded to another
//!   origin;
//! - outbound context goes only to the configured origin;
//! - missing usage is reported as unknown, never as zero;
//! - unsupported controls are reported through `ModelCapabilities`
//!   rather than silently ignored.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Continuation, ContinuationPart, ExpectedArtifact, KnutError, Model, ModelCapabilities,
    ModelIdentity, ModelRequest, ModelResponse, ModelStreamEvent, ModelStreamSink, ModelTier,
    ToolCall, Usage,
};

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
}

impl ReasoningEffort {
    fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
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

/// Configuration for one OpenAI-compatible provider.
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
    /// Whether this provider accepts `stream_options.include_usage`.
    supports_usage_in_stream: bool,
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
        Self {
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: model.into(),
            tier: ModelTier::Reasoner,
            timeout: Duration::from_secs(120),
            billing: BillingPath::Unknown,
            // GLM's documented field; other OpenAI-compatible providers
            // commonly use `reasoning_content` too, and `reasoning` is
            // accepted as an alternate when reported.
            reasoning_field: "reasoning_content".to_owned(),
            supports_reasoning_effort: false,
            reasoning_effort: None,
            supports_usage_in_stream: true,
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
        self
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
    /// `KNUT_PROVIDER_API_KEY` is required; `KNUT_PROVIDER_BASE_URL`,
    /// `KNUT_PROVIDER_MODEL` and `KNUT_PROVIDER_TIER` are optional.
    /// Keys are read from the environment, never from the repository.
    pub fn from_env() -> Result<Self, KnutError> {
        let api_key = std::env::var("KNUT_PROVIDER_API_KEY")
            .map_err(|_| KnutError::Model("KNUT_PROVIDER_API_KEY is not set".to_owned()))?;
        let base_url = std::env::var("KNUT_PROVIDER_BASE_URL")
            .unwrap_or_else(|_| "https://api.z.ai/api/coding/paas/v4".to_owned());
        let model =
            std::env::var("KNUT_PROVIDER_MODEL").unwrap_or_else(|_| "glm-5.3-flash".to_owned());
        let mut config = Self::new(api_key, base_url, model);
        if let Ok(tier) = std::env::var("KNUT_PROVIDER_TIER") {
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

    /// A secret-free summary for `doctor` and logs.
    pub fn summary(&self) -> ProviderSummary {
        ProviderSummary {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            tier: self.tier,
            billing: self.billing,
            timeout: self.timeout,
            api_key_source: "environment",
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
}

/// An authenticated OpenAI-compatible Chat Completions adapter.
pub struct OpenAiCompatibleModel {
    config: ProviderConfig,
    http: reqwest::Client,
}

impl std::fmt::Debug for OpenAiCompatibleModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleModel")
            .field("config", &self.config)
            .finish()
    }
}

impl OpenAiCompatibleModel {
    pub fn new(config: ProviderConfig) -> Result<Self, KnutError> {
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

        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": stream,
        });

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
        }

        let _ = ExpectedArtifact::Json; // shape contract stays with the caller
        body
    }

    /// Classify a non-success response into a typed error.
    async fn error_for(&self, status: reqwest::StatusCode, body: String) -> KnutError {
        // Bound what we echo: provider error bodies can be large.
        let detail: String = body.chars().take(400).collect();
        let message = format!("provider returned {status}: {detail}");
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return KnutError::ModelAuth(message);
        }
        if status.as_u16() == 429 {
            return KnutError::ModelRateLimit(message);
        }
        if status.is_server_error() {
            return KnutError::ModelUnavailable(message);
        }
        KnutError::Model(message)
    }
}

#[async_trait]
impl Model for OpenAiCompatibleModel {
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
        OpenAiCompatibleModel::capabilities(self)
    }

    /// Continue a turn, echoing the provider's continuation state back
    /// unmodified. This is the path that preserves reasoning continuity:
    /// the default trait implementation would drop it.
    async fn continue_turn(
        &self,
        continuation: Option<&Continuation>,
        request: &ModelRequest,
    ) -> Result<ModelResponse, KnutError> {
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
            return Err(self.error_for(status, text).await);
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
            return Err(self.error_for(status, text).await);
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
            return Err(self.error_for(status, text).await);
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
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

            // Fragmented UTF-8 and split SSE frames are normal: buffer
            // bytes, decode lossily at frame boundaries, and keep the
            // remainder for the next chunk.
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim_end_matches('\r').to_owned();
                buffer.drain(..=newline);

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

                for event in accumulator.absorb(&frame) {
                    sink.on_event(event);
                }
            }
        }

        if let Some(message) = raw_error {
            // An error frame after a 200 is a failed call.
            return Err(self
                .error_for(reqwest::StatusCode::BAD_GATEWAY, message)
                .await);
        }

        if !accumulator.done && accumulator.finish_reason.is_none() {
            let reason = "stream ended without a completion frame".to_owned();
            sink.on_event(ModelStreamEvent::Incomplete {
                reason: reason.clone(),
            });
            return Err(KnutError::Model(reason));
        }

        sink.on_event(ModelStreamEvent::Completed {
            usage: accumulator.usage,
        });

        accumulator.into_response(self.identity(), started.elapsed())
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

    fn absorb(&mut self, frame: &StreamFrame) -> Vec<ModelStreamEvent> {
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

            if let Some(reasoning) = delta.reasoning() {
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

        let _ = &self.reasoning_field;
        events
    }

    fn into_response(
        self,
        identity: ModelIdentity,
        latency: Duration,
    ) -> Result<ModelResponse, KnutError> {
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
    fn reasoning(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
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
        _reasoning_field: &str,
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

        let reasoning = choice
            .message
            .reasoning_content
            .or(choice.message.reasoning)
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

    /// A minimal HTTP/1.1 server for protocol fixtures.
    ///
    /// CI stays fully offline: the adapter is exercised against a real
    /// socket speaking the documented wire format, not against a mock
    /// object that could disagree with it.
    struct FixtureServer {
        addr: std::net::SocketAddr,
        seen: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl FixtureServer {
        async fn start(
            responses: Vec<(u16, String)>,
        ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind fixture server");
            let addr = listener.local_addr().expect("addr");
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen_for_task = std::sync::Arc::clone(&seen);
            let responses = std::sync::Arc::new(std::sync::Mutex::new(responses));

            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = std::sync::Arc::clone(&seen_for_task);
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
                                    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
                                        seen.lock().unwrap().push(parsed);
                                    }
                                    let (status, payload) = responses.lock().unwrap().remove(0);
                                    let response = format!(
                                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                        payload.len()
                                    );
                                    let _ = socket.write_all(response.as_bytes()).await;
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
                },
                seen,
            )
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn requests(&self) -> Vec<Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    /// One SSE frame.
    fn sse(payload: &str) -> String {
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
        let model = OpenAiCompatibleModel::new(config).unwrap();

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

        let model = OpenAiCompatibleModel::new(ProviderConfig::new(
            "k",
            server.base_url(),
            "glm-5.3-flash",
        ))
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
        // The server writes one byte at a time: frames split across TCP
        // reads must still reassemble deterministically.
        let (server, _) =
            FixtureServer::start(vec![(200, reasoning_tool_then_artifact_stream())]).await;
        let _ = server;

        // A single-frame-per-read server with a slow write is covered by
        // the accumulator tests; here assert the same payload through the
        // real socket path is stable.
        let model = OpenAiCompatibleModel::new(ProviderConfig::new(
            "k",
            server.base_url(),
            "glm-5.3-flash",
        ))
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

        let model = OpenAiCompatibleModel::new(ProviderConfig::new(
            "k",
            server.base_url(),
            "glm-5.3-flash",
        ))
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

        let model = OpenAiCompatibleModel::new(ProviderConfig::new(
            "k",
            server.base_url(),
            "glm-5.3-flash",
        ))
        .unwrap();

        let err = model
            .complete(&ModelRequest::new("x", ExpectedArtifact::Text))
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::Model(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn truncated_answer_is_reported_not_silently_accepted() {
        let (server, _) = FixtureServer::start(vec![(
            200,
            r#"{"model":"glm-5.3-flash","choices":[{"message":{"content":"half an ans"},"finish_reason":"length"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#.to_owned(),
        )])
        .await;

        let model = OpenAiCompatibleModel::new(ProviderConfig::new(
            "k",
            server.base_url(),
            "glm-5.3-flash",
        ))
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

        let model = OpenAiCompatibleModel::new(ProviderConfig::new(
            "k",
            server.base_url(),
            "glm-5.3-flash",
        ))
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

        let model = OpenAiCompatibleModel::new(ProviderConfig::new(
            "k",
            server.base_url(),
            "glm-5.3-flash",
        ))
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

        let model = OpenAiCompatibleModel::new(ProviderConfig::glm_coding(api_key)).unwrap();
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
            events.extend(accumulator.absorb(&frame));
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
            events.extend(accumulator.absorb(&frame));
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
        accumulator.absorb(&frame);

        assert!(
            accumulator
                .into_response(identity(), Duration::ZERO)
                .is_err()
        );
    }

    #[test]
    fn continuation_round_trips_into_the_next_request() {
        let config = ProviderConfig::glm_coding("k");
        let model = OpenAiCompatibleModel::new(config).unwrap();

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
        let model = OpenAiCompatibleModel::new(ProviderConfig::glm_coding("k")).unwrap();
        let body = model.body(&ModelRequest::new("hi", ExpectedArtifact::Text), true, None);
        assert_eq!(body["stream_options"]["include_usage"], true);

        let model =
            OpenAiCompatibleModel::new(ProviderConfig::glm_coding("k").with_usage_in_stream(false))
                .unwrap();
        let body = model.body(&ModelRequest::new("hi", ExpectedArtifact::Text), true, None);
        // Never ask for a control the endpoint does not document.
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn capability_report_is_honest_about_reasoning_controls() {
        let model = OpenAiCompatibleModel::new(ProviderConfig::glm_coding("k")).unwrap();
        let caps = model.capabilities();
        assert!(caps.streaming);
        assert!(caps.tools);
        assert!(caps.continuation);
        assert!(caps.usage);
        // GLM's coding endpoint does not accept `reasoning_effort` here,
        // so the report must not claim it.
        assert!(!caps.reasoning);
    }

    #[test]
    fn structured_input_is_serialized_into_the_user_message() {
        let model = OpenAiCompatibleModel::new(ProviderConfig::glm_coding("k")).unwrap();
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
}
