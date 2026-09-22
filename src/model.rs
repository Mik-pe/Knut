use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{KnutError, ModelTier};

/// The artifact a call site expects, so models answer a bounded task
/// rather than a free-form prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedArtifact {
    Text,
    Json,
}

/// Provider/model identity, exposed on every response for tracing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub provider: String,
    pub model: String,
    pub tier: ModelTier,
}

/// Token accounting for one call.
///
/// A provider that does not report usage is *unknown*, not free: the
/// fields are optional so nothing downstream can silently treat a
/// missing count as zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    /// Fully reported usage.
    pub fn known(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
        }
    }

    /// Whether the provider reported any usage at all.
    pub fn is_known(&self) -> bool {
        self.input_tokens.is_some() || self.output_tokens.is_some()
    }

    /// Sum two reports, treating "unknown" as unknown rather than zero.
    pub fn merge(self, other: Self) -> Self {
        fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (Some(a), Some(b)) => a.checked_add(b),
                _ => None,
            }
        }
        Self {
            input_tokens: add(self.input_tokens, other.input_tokens),
            output_tokens: add(self.output_tokens, other.output_tokens),
        }
    }
}

/// A bounded generative task.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    pub purpose: crate::CallPurpose,
    pub instruction: String,
    pub expected_artifact: ExpectedArtifact,
    /// Structured input/context for the task.
    pub input: Value,
}

impl ModelRequest {
    pub fn new(instruction: impl Into<String>, expected_artifact: ExpectedArtifact) -> Self {
        Self {
            purpose: crate::CallPurpose::Response,
            instruction: instruction.into(),
            expected_artifact,
            input: Value::Null,
        }
    }

    pub fn with_input(mut self, input: Value) -> Self {
        self.input = input;
        self
    }

    pub fn with_purpose(mut self, purpose: crate::CallPurpose) -> Self {
        self.purpose = purpose;
        self
    }
}

/// One completed model call with full provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelResponse {
    pub content: String,
    pub identity: ModelIdentity,
    pub usage: Usage,
    /// Wall-clock latency of the call itself.
    pub latency: std::time::Duration,
    /// Tool calls the model requested, arguments already parsed.
    pub tool_calls: Vec<ToolCall>,
    /// Provider continuation state to echo back verbatim, if any.
    pub continuation: Continuation,
}

impl ModelResponse {
    /// A text-only response (the common case for adapters and fakes).
    pub fn text(
        content: impl Into<String>,
        identity: ModelIdentity,
        usage: Usage,
        latency: std::time::Duration,
    ) -> Self {
        Self {
            content: content.into(),
            identity,
            usage,
            latency,
            tool_calls: Vec::new(),
            continuation: Continuation::new(),
        }
    }
}

/// A provider adapter for one capability tier.
///
/// Implementations are configuration: the runtime never names providers.
#[async_trait]
pub trait Model: Send + Sync {
    fn identity(&self) -> ModelIdentity;

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError>;

    /// What this adapter can actually do.
    ///
    /// Defaults describe the minimal adapter (buffered text only), so an
    /// adapter that supports streaming, tools or reasoning must say so
    /// explicitly instead of being assumed to.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::buffered_text()
    }

    /// Stream one turn as typed events.
    ///
    /// The default implementation wraps a buffered call, so existing
    /// adapters keep working; a real streaming adapter overrides it.
    /// Callers that need incremental output use this path.
    async fn stream(
        &self,
        request: &ModelRequest,
        sink: &mut (dyn ModelStreamSink + Send),
    ) -> Result<ModelResponse, KnutError> {
        let response = self.complete(request).await?;
        sink.on_event(ModelStreamEvent::TextDelta {
            text: response.content.clone(),
        });
        sink.on_event(ModelStreamEvent::Completed {
            usage: response.usage,
        });
        Ok(response)
    }

    /// Continue a conversation, preserving provider continuation state.
    ///
    /// Providers that require an opaque reasoning/continuation token
    /// (see [`ModelResponse::continuation`]) get it back here. The
    /// default implementation ignores it and replays the last turn as a
    /// single message: adapters that need more must override this rather
    /// than silently losing reasoning continuity.
    async fn continue_turn(
        &self,
        continuation: Option<&Continuation>,
        request: &ModelRequest,
    ) -> Result<ModelResponse, KnutError> {
        let _ = continuation;
        self.complete(request).await
    }
}

/// What an adapter supports. Reported, never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Incremental text/tool-call events.
    pub streaming: bool,
    /// Tool calling.
    pub tools: bool,
    /// Provider-specific reasoning controls (effort, thinking budget).
    pub reasoning: bool,
    /// Provider continuation state that must be echoed back verbatim.
    pub continuation: bool,
    /// A real (not defaulted) usage report.
    pub usage: bool,
}

impl ModelCapabilities {
    /// The conservative default: whatever a buffered text adapter can
    /// honestly claim.
    pub const fn buffered_text() -> Self {
        Self {
            streaming: false,
            tools: false,
            reasoning: false,
            continuation: false,
            usage: false,
        }
    }
}

/// Opaque provider continuation state (e.g. GLM `reasoning_content`).
///
/// Held as an ordered list of raw provider parts: it is echoed back
/// unmodified and never summarized or routed through another system.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Continuation {
    pub parts: Vec<ContinuationPart>,
}

impl Continuation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

/// One provider part preserved verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationPart {
    /// Provider-defined kind (e.g. "reasoning_content").
    pub kind: String,
    pub value: String,
}

/// One incremental event in a model turn.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelStreamEvent {
    /// A fragment of assistant text. Fragments are not necessarily
    /// valid UTF-8 boundaries or words; consumers append them.
    TextDelta { text: String },
    /// A tool call began. Arguments arrive as deltas afterwards.
    ToolCallStarted { id: String, name: String },
    /// A fragment of a tool call's arguments. A partial JSON fragment is
    /// **not** executable: the sink must buffer until `ToolCallEnded`.
    ToolCallArgumentsDelta { id: String, delta: String },
    /// The tool call's arguments are complete and schema-checkable.
    ToolCallEnded { id: String },
    /// Provider reasoning text, preserved for continuation, never
    /// synthesized by us.
    ReasoningDelta { text: String },
    /// The turn finished successfully.
    Completed { usage: Usage },
    /// The turn ended without a complete artifact (transport drop, EOF).
    Incomplete { reason: String },
}

/// Receives streaming events as they arrive.
///
/// Implementations must not block: this is called from the transport
/// read loop.
pub trait ModelStreamSink: Send + Sync {
    fn on_event(&self, event: ModelStreamEvent);
}

/// A sink that buffers the whole turn (the simplest consumer).
#[derive(Debug, Default)]
pub struct BufferedSink {
    events: std::sync::Mutex<Vec<ModelStreamEvent>>,
}

impl ModelStreamSink for BufferedSink {
    fn on_event(&self, event: ModelStreamEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

impl BufferedSink {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn events(&self) -> Vec<ModelStreamEvent> {
        self.events.lock().map(|e| e.clone()).unwrap_or_default()
    }

    /// Reassemble the turn into a completed response.
    ///
    /// Returns `None` for an incomplete stream: a partially streamed
    /// turn must never become a successful artifact.
    pub fn into_response(
        self,
        identity: ModelIdentity,
        latency: Duration,
    ) -> Option<ModelResponse> {
        let events = self.events.into_inner().ok()?;
        let mut content = String::new();
        let mut reasoning = Vec::new();
        let mut calls: std::collections::BTreeMap<String, (String, String)> =
            std::collections::BTreeMap::new();
        let mut order: Vec<(String, String)> = Vec::new();
        let mut usage = None;
        let mut completed = false;

        for event in events {
            match event {
                ModelStreamEvent::TextDelta { text } => content.push_str(&text),
                ModelStreamEvent::ReasoningDelta { text } => reasoning.push(text),
                ModelStreamEvent::ToolCallStarted { id, name } => {
                    calls.insert(id.clone(), (name.clone(), String::new()));
                    order.push((id, name));
                }
                ModelStreamEvent::ToolCallArgumentsDelta { id, delta } => {
                    if let Some((_, arguments)) = calls.get_mut(&id) {
                        arguments.push_str(&delta);
                    }
                }
                ModelStreamEvent::ToolCallEnded { .. } => {}
                ModelStreamEvent::Completed { usage: reported } => {
                    usage = Some(reported);
                    completed = true;
                }
                ModelStreamEvent::Incomplete { .. } => return None,
            }
        }

        if !completed {
            // No explicit completion: the stream was cut short.
            return None;
        }

        let tool_calls = order
            .into_iter()
            .map(|(id, name)| {
                let (_, arguments) = calls.get(&id).cloned().unwrap_or_default();
                ToolCall {
                    id,
                    name,
                    arguments: serde_json::from_str(&arguments).unwrap_or(Value::Null),
                }
            })
            .collect();

        Some(ModelResponse {
            content,
            identity,
            usage: usage.unwrap_or_default(),
            latency,
            tool_calls,
            continuation: if reasoning.is_empty() {
                Continuation::new()
            } else {
                Continuation {
                    parts: reasoning
                        .into_iter()
                        .map(|value| ContinuationPart {
                            kind: "reasoning_content".to_owned(),
                            value,
                        })
                        .collect(),
                }
            },
        })
    }
}

/// One tool call the model asked for, with arguments already parsed.
///
/// Arguments are only ever produced completed and validated: the stream
/// carries fragments, this type carries the finished, parsed object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Attach the previous attempt's failure to an escalated request.
///
/// The stronger model must see the exact earlier failure and evidence,
/// not merely the original instruction again.
fn escalate_request(request: &ModelRequest, feedback: Option<&Value>) -> ModelRequest {
    let Some(feedback) = feedback else {
        return request.clone();
    };

    let mut input = match request.input.clone() {
        Value::Object(map) => Value::Object(map),
        Value::Null => serde_json::json!({}),
        other => serde_json::json!({ "previous_input": other }),
    };
    if let Value::Object(map) = &mut input {
        map.insert("previous_attempt".to_owned(), feedback.clone());
    }

    request.clone().with_input(input)
}

/// Sharing a model (for tests, counters, or one adapter behind several
/// tiers) must not require owning it.
#[async_trait]
impl<T: Model + ?Sized> Model for std::sync::Arc<T> {
    fn identity(&self) -> ModelIdentity {
        (**self).identity()
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError> {
        (**self).complete(request).await
    }
}

/// The verdict of a verification pass over a model response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationVerdict {
    Sufficient,
    Retry { reason: String },
}

/// Decides whether a response is good enough for the caller's purpose.
pub trait Verifier: Send + Sync {
    fn verify(&self, response: &ModelResponse) -> VerificationVerdict;
}

/// One escalation step, recorded for traces and evals.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelAttempt {
    pub tier: ModelTier,
    pub identity: ModelIdentity,
    pub usage: Usage,
    pub latency: std::time::Duration,
    pub verdict: Option<VerificationVerdict>,
}

/// The full story of one cascade run.
#[derive(Debug, Clone, PartialEq)]
pub struct CascadeOutcome {
    pub response: ModelResponse,
    pub attempts: Vec<ModelAttempt>,
    /// Why each escalation happened, oldest first.
    pub escalation_reasons: Vec<String>,
}

/// Cheap-first compute cascade over one model per tier.
///
/// The cascade tries the requested tier, then escalates up the ladder
/// (Fast -> Standard -> Reasoner) while verification says Retry and tiers
/// remain. The ladder itself bounds the attempts: there is no retry
/// counter to misconfigure and no hidden call multiplier.
#[derive(Default)]
pub struct ComputeCascade {
    fast: Option<Box<dyn Model>>,
    standard: Option<Box<dyn Model>>,
    reasoner: Option<Box<dyn Model>>,
}

impl ComputeCascade {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_fast(mut self, model: impl Model + 'static) -> Self {
        self.fast = Some(Box::new(model));
        self
    }

    pub fn with_standard(mut self, model: impl Model + 'static) -> Self {
        self.standard = Some(Box::new(model));
        self
    }

    pub fn with_reasoner(mut self, model: impl Model + 'static) -> Self {
        self.reasoner = Some(Box::new(model));
        self
    }

    fn model_for(&self, tier: ModelTier) -> Option<&dyn Model> {
        match tier {
            ModelTier::Fast => self.fast.as_deref(),
            ModelTier::Standard => self.standard.as_deref(),
            ModelTier::Reasoner => self.reasoner.as_deref(),
        }
    }

    /// Whether this cascade can serve a given tier.
    ///
    /// Public because "the reasoner is configured" and "the runtime can
    /// actually generate" are different claims, and a client that shows
    /// the first should be able to check the second.
    pub fn has_model_for(&self, tier: ModelTier) -> bool {
        self.model_for(tier).is_some()
    }

    fn next_tier(tier: ModelTier) -> Option<ModelTier> {
        match tier {
            ModelTier::Fast => Some(ModelTier::Standard),
            ModelTier::Standard => Some(ModelTier::Reasoner),
            ModelTier::Reasoner => None,
        }
    }

    /// Run the bounded task starting at `starting_tier`, escalating until
    /// verification passes or the ladder runs out, streaming each
    /// attempt's events to `sink`.
    ///
    /// Escalation semantics are identical to [`ComputeCascade::run`];
    /// this variant exists so a UI can render a turn as it arrives.
    /// A stream that ends without completion is an error, never an
    /// empty successful response.
    pub async fn run_streaming(
        &self,
        request: &ModelRequest,
        starting_tier: ModelTier,
        verifier: &dyn Verifier,
        sink: &mut (dyn ModelStreamSink + Send),
    ) -> Result<CascadeOutcome, KnutError> {
        let mut attempts = Vec::new();
        let mut escalation_reasons = Vec::new();
        let mut tier = starting_tier;
        let mut request = request.clone();
        let mut last_failure: Option<String> = None;

        loop {
            let Some(model) = self.model_for(tier) else {
                match Self::next_tier(tier) {
                    Some(next) => {
                        escalation_reasons
                            .push(format!("{tier:?} tier not configured; escalating"));
                        tier = next;
                        continue;
                    }
                    None => {
                        let reason = last_failure
                            .clone()
                            .unwrap_or_else(|| "no model configured for any tier".to_owned());
                        return Err(KnutError::ModelExhausted { reason, attempts });
                    }
                }
            };

            let capabilities = model.capabilities();
            let measurement = crate::census::CallMeasurement::start(
                &request,
                model.identity(),
                tier,
                capabilities.streaming,
            );
            let started = Instant::now();
            let response = if capabilities.streaming {
                model.stream(&request, sink).await
            } else {
                // A non-streaming adapter still works: it reports the
                // whole turn as one delta, and never claims otherwise.
                model.complete(&request).await.inspect(|response| {
                    sink.on_event(ModelStreamEvent::TextDelta {
                        text: response.content.clone(),
                    });
                    sink.on_event(ModelStreamEvent::Completed {
                        usage: response.usage,
                    });
                })
            };
            let latency = started.elapsed();

            measurement.finish(&response);

            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    last_failure = Some(format!("{tier:?} call failed: {err}"));
                    attempts.push(ModelAttempt {
                        tier,
                        identity: model.identity(),
                        usage: Usage::default(),
                        latency,
                        verdict: Some(VerificationVerdict::Retry {
                            reason: err.to_string(),
                        }),
                    });
                    match Self::next_tier(tier) {
                        Some(next) => {
                            escalation_reasons
                                .push(format!("{tier:?} call failed ({err}); escalating"));
                            let feedback = serde_json::json!({
                                "failed_tier": format!("{tier:?}"),
                                "failure": err.to_string(),
                            });
                            request = escalate_request(&request, Some(&feedback));
                            tier = next;
                            continue;
                        }
                        None => {
                            return Err(KnutError::ModelExhausted {
                                reason: format!("{tier:?} call failed: {err}"),
                                attempts,
                            });
                        }
                    }
                }
            };

            let verdict = verifier.verify(&response);
            attempts.push(ModelAttempt {
                tier,
                identity: response.identity.clone(),
                usage: response.usage,
                latency,
                verdict: Some(verdict.clone()),
            });

            match verdict {
                VerificationVerdict::Sufficient => {
                    return Ok(CascadeOutcome {
                        response,
                        attempts,
                        escalation_reasons,
                    });
                }
                VerificationVerdict::Retry { reason } => match Self::next_tier(tier) {
                    Some(next) => {
                        last_failure = Some(reason.clone());
                        escalation_reasons.push(reason.clone());
                        let feedback = serde_json::json!({
                            "failed_tier": format!("{tier:?}"),
                            "rejected_artifact": response.content,
                            "verifier_reason": reason,
                        });
                        request = escalate_request(&request, Some(&feedback));
                        tier = next;
                    }
                    None => {
                        return Err(KnutError::ModelExhausted { reason, attempts });
                    }
                },
            }
        }
    }

    /// Run the bounded task starting at `starting_tier`, escalating until
    /// verification passes or the ladder runs out.
    ///
    /// A stronger tier sees *why* the weaker one was rejected: the failed
    /// response and the verifier's reason are attached to the escalated
    /// request instead of replaying the original instruction unchanged.
    pub async fn run(
        &self,
        request: &ModelRequest,
        starting_tier: ModelTier,
        verifier: &dyn Verifier,
    ) -> Result<CascadeOutcome, KnutError> {
        let mut attempts = Vec::new();
        let mut escalation_reasons = Vec::new();
        let mut tier = starting_tier;
        let mut request = request.clone();
        // The most recent *real* failure (transport or verification), so a
        // terminal error names it instead of a routing detail.
        let mut last_failure: Option<String> = None;

        loop {
            let Some(model) = self.model_for(tier) else {
                // Configured away: climb without counting a failed attempt.
                match Self::next_tier(tier) {
                    Some(next) => {
                        escalation_reasons
                            .push(format!("{tier:?} tier not configured; escalating"));
                        tier = next;
                        continue;
                    }
                    None => {
                        // Name the real reason when there is one: a
                        // failed call that cannot escalate must not be
                        // reported as "nothing was configured".
                        let reason = last_failure
                            .clone()
                            .unwrap_or_else(|| "no model configured for any tier".to_owned());
                        return Err(KnutError::ModelExhausted { reason, attempts });
                    }
                }
            };

            let started = Instant::now();
            let measurement =
                crate::census::CallMeasurement::start(&request, model.identity(), tier, false);
            let result = model.complete(&request).await;
            measurement.finish(&result);
            let response = match result {
                Ok(response) => response,
                Err(err) => {
                    // A transport failure is an *attempt*, not a silent
                    // gap: record it with the identity we can name, then
                    // escalate to a stronger tier rather than pretending
                    // the call never happened.
                    attempts.push(ModelAttempt {
                        tier,
                        identity: model.identity(),
                        usage: Usage::default(),
                        latency: started.elapsed(),
                        verdict: Some(VerificationVerdict::Retry {
                            reason: err.to_string(),
                        }),
                    });
                    last_failure = Some(format!("{tier:?} call failed: {err}"));
                    match Self::next_tier(tier) {
                        Some(next) => {
                            escalation_reasons
                                .push(format!("{tier:?} call failed ({err}); escalating"));
                            let feedback = serde_json::json!({
                                "failed_tier": format!("{tier:?}"),
                                "failure": err.to_string(),
                            });
                            request = escalate_request(&request, Some(&feedback));
                            tier = next;
                            continue;
                        }
                        None => {
                            return Err(KnutError::ModelExhausted {
                                reason: format!("{tier:?} call failed: {err}"),
                                attempts,
                            });
                        }
                    }
                }
            };
            let latency = started.elapsed();

            let verdict = verifier.verify(&response);
            let attempt = ModelAttempt {
                tier,
                identity: response.identity.clone(),
                usage: response.usage,
                latency,
                verdict: Some(verdict.clone()),
            };
            attempts.push(attempt);

            match verdict {
                VerificationVerdict::Sufficient => {
                    return Ok(CascadeOutcome {
                        response,
                        attempts,
                        escalation_reasons,
                    });
                }
                VerificationVerdict::Retry { reason } => match Self::next_tier(tier) {
                    Some(next) => {
                        last_failure = Some(reason.clone());
                        escalation_reasons.push(reason.clone());
                        let feedback = serde_json::json!({
                            "failed_tier": format!("{tier:?}"),
                            "rejected_artifact": response.content,
                            "verifier_reason": reason,
                        });
                        request = escalate_request(&request, Some(&feedback));
                        tier = next;
                    }
                    None => {
                        return Err(KnutError::ModelExhausted { reason, attempts });
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;

    /// Deterministic fake: scripted content, records what it was asked.
    struct FakeModel {
        identity: ModelIdentity,
        contents: Vec<String>,
        calls: Arc<AtomicUsize>,
        seen_artifacts: Arc<std::sync::Mutex<Vec<ExpectedArtifact>>>,
    }

    impl FakeModel {
        fn always_ok(tier: ModelTier) -> Self {
            Self::scripted(tier, vec!["answer".to_owned()])
        }

        fn scripted(tier: ModelTier, contents: Vec<String>) -> Self {
            Self {
                identity: ModelIdentity {
                    provider: "fake".to_owned(),
                    model: format!("fake-{tier:?}").to_lowercase(),
                    tier,
                },
                contents,
                calls: Arc::new(AtomicUsize::new(0)),
                seen_artifacts: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl Model for FakeModel {
        fn identity(&self) -> ModelIdentity {
            self.identity.clone()
        }

        async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError> {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_artifacts
                .lock()
                .unwrap()
                .push(request.expected_artifact);

            let content = self
                .contents
                .get(index)
                .cloned()
                .unwrap_or_else(|| "fallback".to_owned());

            Ok(ModelResponse::text(
                content,
                self.identity.clone(),
                Usage::known(10, 5),
                std::time::Duration::from_millis(1),
            ))
        }
    }

    /// Verification script: verdicts consumed one per call.
    struct ScriptedVerifier {
        verdicts: Vec<VerificationVerdict>,
        index: std::sync::Mutex<usize>,
    }

    impl ScriptedVerifier {
        fn new(verdicts: Vec<VerificationVerdict>) -> Self {
            Self {
                verdicts,
                index: std::sync::Mutex::new(0),
            }
        }
    }

    impl Verifier for ScriptedVerifier {
        fn verify(&self, _response: &ModelResponse) -> VerificationVerdict {
            let mut index = self.index.lock().unwrap();
            let verdict = self
                .verdicts
                .get(*index)
                .cloned()
                .unwrap_or(VerificationVerdict::Sufficient);
            *index += 1;
            verdict
        }
    }

    fn retry(reason: &str) -> VerificationVerdict {
        VerificationVerdict::Retry {
            reason: reason.to_owned(),
        }
    }

    #[tokio::test]
    async fn cheap_first_stays_cheap_when_sufficient() {
        let cascade = ComputeCascade::empty()
            .with_fast(FakeModel::always_ok(ModelTier::Fast))
            .with_reasoner(FakeModel::always_ok(ModelTier::Reasoner));

        let outcome = cascade
            .run(
                &ModelRequest::new("summarize", ExpectedArtifact::Text),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![VerificationVerdict::Sufficient]),
            )
            .await
            .unwrap();

        assert_eq!(outcome.response.identity.tier, ModelTier::Fast);
        assert_eq!(outcome.attempts.len(), 1);
        assert!(outcome.escalation_reasons.is_empty());
    }

    #[tokio::test]
    async fn failed_verification_escalates_to_reasoner() {
        let cascade = ComputeCascade::empty()
            .with_fast(FakeModel::always_ok(ModelTier::Fast))
            .with_reasoner(FakeModel::always_ok(ModelTier::Reasoner));

        let outcome = cascade
            .run(
                &ModelRequest::new("hard math", ExpectedArtifact::Text),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![
                    retry("wrong answer"),
                    VerificationVerdict::Sufficient,
                ]),
            )
            .await
            .unwrap();

        assert_eq!(outcome.response.identity.tier, ModelTier::Reasoner);
        assert_eq!(outcome.attempts.len(), 2);
        assert_eq!(
            outcome.escalation_reasons,
            vec![
                "wrong answer".to_owned(),
                "Standard tier not configured; escalating".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn unconfigured_tiers_are_skipped() {
        let cascade = ComputeCascade::empty()
            .with_fast(FakeModel::always_ok(ModelTier::Fast))
            .with_reasoner(FakeModel::always_ok(ModelTier::Reasoner));

        // Start at Standard, which has no model: must jump to Reasoner.
        let outcome = cascade
            .run(
                &ModelRequest::new("task", ExpectedArtifact::Text),
                ModelTier::Standard,
                &ScriptedVerifier::new(vec![VerificationVerdict::Sufficient]),
            )
            .await
            .unwrap();

        assert_eq!(outcome.response.identity.tier, ModelTier::Reasoner);
        assert_eq!(outcome.attempts.len(), 1);
        assert_eq!(
            outcome.escalation_reasons,
            vec!["Standard tier not configured; escalating".to_owned()]
        );
    }

    #[tokio::test]
    async fn exhausted_ladder_fails_with_attempts_recorded() {
        let cascade = ComputeCascade::empty()
            .with_fast(FakeModel::always_ok(ModelTier::Fast))
            .with_standard(FakeModel::always_ok(ModelTier::Standard))
            .with_reasoner(FakeModel::always_ok(ModelTier::Reasoner));

        let err = cascade
            .run(
                &ModelRequest::new("impossible", ExpectedArtifact::Text),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![retry("no"), retry("still no"), retry("never")]),
            )
            .await
            .unwrap_err();

        match err {
            KnutError::ModelExhausted { reason, attempts } => {
                assert_eq!(reason, "never");
                assert_eq!(attempts.len(), 3);
                assert_eq!(attempts[0].tier, ModelTier::Fast);
                assert_eq!(attempts[2].tier, ModelTier::Reasoner);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn responses_carry_usage_and_provenance() {
        let cascade = ComputeCascade::empty().with_fast(FakeModel::always_ok(ModelTier::Fast));

        let outcome = cascade
            .run(
                &ModelRequest::new("hi", ExpectedArtifact::Json),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![VerificationVerdict::Sufficient]),
            )
            .await
            .unwrap();

        assert_eq!(outcome.response.usage, Usage::known(10, 5));
        assert_eq!(outcome.response.identity.provider, "fake");
        assert!(outcome.response.latency >= std::time::Duration::from_millis(1));
        assert_eq!(outcome.response.content, "answer");
    }

    #[tokio::test]
    async fn requests_carry_expected_artifact() {
        let model = FakeModel::always_ok(ModelTier::Fast);
        let seen = Arc::clone(&model.seen_artifacts);

        let cascade = ComputeCascade::empty().with_fast(model);
        cascade
            .run(
                &ModelRequest::new("give json", ExpectedArtifact::Json),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![VerificationVerdict::Sufficient]),
            )
            .await
            .unwrap();

        assert_eq!(*seen.lock().unwrap(), vec![ExpectedArtifact::Json]);
    }

    #[tokio::test]
    async fn structured_input_reaches_the_model() {
        struct RecordingModel {
            seen: Arc<std::sync::Mutex<Vec<Value>>>,
        }

        #[async_trait]
        impl Model for RecordingModel {
            fn identity(&self) -> ModelIdentity {
                ModelIdentity {
                    provider: "fake".to_owned(),
                    model: "recorder".to_owned(),
                    tier: ModelTier::Fast,
                }
            }

            async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError> {
                self.seen.lock().unwrap().push(request.input.clone());
                Ok(ModelResponse::text(
                    "ok".to_owned(),
                    self.identity(),
                    Usage::known(0, 0),
                    std::time::Duration::ZERO,
                ))
            }
        }

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cascade = ComputeCascade::empty().with_fast(RecordingModel {
            seen: Arc::clone(&seen),
        });

        cascade
            .run(
                &ModelRequest::new("task", ExpectedArtifact::Text)
                    .with_input(json!({ "code": "let x = 1;" })),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![VerificationVerdict::Sufficient]),
            )
            .await
            .unwrap();

        assert_eq!(*seen.lock().unwrap(), vec![json!({ "code": "let x = 1;" })]);
    }

    // --- streaming (#20) -------------------------------------------------

    /// A streaming adapter that emits a scripted event sequence.
    struct ScriptedStreamer {
        events: std::sync::Mutex<Vec<ModelStreamEvent>>,
    }

    impl ScriptedStreamer {
        fn new(events: Vec<ModelStreamEvent>) -> Self {
            Self {
                events: std::sync::Mutex::new(events),
            }
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                streaming: true,
                tools: true,
                reasoning: true,
                continuation: true,
                usage: true,
            }
        }
    }

    #[async_trait]
    impl Model for ScriptedStreamer {
        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: "fake".to_owned(),
                model: "streamer".to_owned(),
                tier: ModelTier::Reasoner,
            }
        }

        async fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse, KnutError> {
            unimplemented!("this fake only streams")
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                streaming: true,
                tools: true,
                reasoning: true,
                continuation: true,
                usage: true,
            }
        }

        async fn stream(
            &self,
            _request: &ModelRequest,
            sink: &mut (dyn ModelStreamSink + Send),
        ) -> Result<ModelResponse, KnutError> {
            let events = self.events.lock().unwrap().clone();
            let buffered = BufferedSink::new();
            for event in events {
                buffered.on_event(event.clone());
                sink.on_event(event);
            }
            Ok(buffered
                .into_response(self.identity(), Duration::ZERO)
                .unwrap_or_else(|| {
                    ModelResponse::text(
                        String::new(),
                        self.identity(),
                        Usage::default(),
                        Duration::ZERO,
                    )
                }))
        }
    }

    #[test]
    fn fragmented_utf8_and_interleaved_tool_deltas_reassemble_deterministically() {
        // Text arrives in byte-fragments (including a multi-byte char
        // split across events) interleaved with tool-call argument
        // deltas for two calls.
        let sink = BufferedSink::new();
        for event in [
            ModelStreamEvent::TextDelta {
                text: "hé".to_owned(),
            },
            ModelStreamEvent::ToolCallStarted {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
            },
            ModelStreamEvent::TextDelta {
                text: "llo".to_owned(),
            },
            ModelStreamEvent::ToolCallArgumentsDelta {
                id: "call_1".to_owned(),
                delta: "{\"path\":".to_owned(),
            },
            ModelStreamEvent::ToolCallStarted {
                id: "call_2".to_owned(),
                name: "grep".to_owned(),
            },
            ModelStreamEvent::ToolCallArgumentsDelta {
                id: "call_1".to_owned(),
                delta: "\"a.rs\"}".to_owned(),
            },
            ModelStreamEvent::ToolCallEnded {
                id: "call_1".to_owned(),
            },
            ModelStreamEvent::ToolCallArgumentsDelta {
                id: "call_2".to_owned(),
                delta: "{\"pattern\":\"x\"}".to_owned(),
            },
            ModelStreamEvent::ToolCallEnded {
                id: "call_2".to_owned(),
            },
            ModelStreamEvent::Completed {
                usage: Usage::known(7, 3),
            },
        ] {
            sink.on_event(event);
        }

        let identity = ModelIdentity {
            provider: "fake".to_owned(),
            model: "streamer".to_owned(),
            tier: ModelTier::Reasoner,
        };
        let response = sink.into_response(identity, Duration::ZERO).unwrap();

        assert_eq!(response.content, "héllo");
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].arguments, json!({ "path": "a.rs" }));
        assert_eq!(response.tool_calls[1].arguments, json!({ "pattern": "x" }));
        assert_eq!(response.usage, Usage::known(7, 3));
    }

    #[test]
    fn incomplete_stream_is_never_a_successful_artifact() {
        // Mid-stream failure: the sink refuses to produce a response, so
        // a broken stream cannot become a patch.
        let sink = BufferedSink::new();
        sink.on_event(ModelStreamEvent::TextDelta {
            text: "partial".to_owned(),
        });
        sink.on_event(ModelStreamEvent::Incomplete {
            reason: "unexpected EOF".to_owned(),
        });

        let identity = ModelIdentity {
            provider: "fake".to_owned(),
            model: "streamer".to_owned(),
            tier: ModelTier::Reasoner,
        };
        assert!(sink.into_response(identity, Duration::ZERO).is_none());

        // A stream that merely stops without completion is also incomplete.
        let sink = BufferedSink::new();
        sink.on_event(ModelStreamEvent::TextDelta {
            text: "no final event".to_owned(),
        });
        let identity = ModelIdentity {
            provider: "fake".to_owned(),
            model: "streamer".to_owned(),
            tier: ModelTier::Reasoner,
        };
        assert!(sink.into_response(identity, Duration::ZERO).is_none());
    }

    #[test]
    fn missing_usage_is_unknown_not_zero() {
        let usage = Usage::default();
        assert!(!usage.is_known());
        assert_ne!(usage.input_tokens, Some(0));

        let merged = usage.merge(Usage::known(5, 5));
        assert_eq!(merged, Usage::default());
        assert_eq!(
            Usage::known(2, 3).merge(Usage::known(4, 5)),
            Usage::known(6, 8)
        );
    }

    #[test]
    fn reasoning_content_is_preserved_verbatim_for_continuation() {
        let sink = BufferedSink::new();
        sink.on_event(ModelStreamEvent::ReasoningDelta {
            text: "step 1: consider the edge case".to_owned(),
        });
        sink.on_event(ModelStreamEvent::ReasoningDelta {
            text: "step 2: pick the simple fix".to_owned(),
        });
        sink.on_event(ModelStreamEvent::TextDelta {
            text: "done".to_owned(),
        });
        sink.on_event(ModelStreamEvent::Completed {
            usage: Usage::known(3, 2),
        });

        let identity = ModelIdentity {
            provider: "fake".to_owned(),
            model: "streamer".to_owned(),
            tier: ModelTier::Reasoner,
        };
        let response = sink.into_response(identity, Duration::ZERO).unwrap();

        assert_eq!(response.continuation.parts.len(), 2);
        assert_eq!(response.continuation.parts[0].kind, "reasoning_content");
        // Unmodified: no summarization, no rewriting.
        assert_eq!(
            response.continuation.parts[0].value,
            "step 1: consider the edge case"
        );
        assert_eq!(
            response.continuation.parts[1].value,
            "step 2: pick the simple fix"
        );
    }

    /// Fails (transport error) or returns content, recording requests.
    struct FlakyModel {
        tier: ModelTier,
        fail: bool,
        content: String,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    #[async_trait]
    impl Model for FlakyModel {
        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: "fake".to_owned(),
                model: format!("flaky-{:?}", self.tier).to_lowercase(),
                tier: self.tier,
            }
        }

        async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError> {
            self.seen.lock().unwrap().push(request.input.clone());
            if self.fail {
                return Err(KnutError::SystemOne("connection reset".to_owned()));
            }
            Ok(ModelResponse::text(
                self.content.clone(),
                self.identity(),
                Usage::known(1, 1),
                Duration::ZERO,
            ))
        }
    }

    #[tokio::test]
    async fn stronger_tier_sees_the_failed_artifact_and_reason() {
        let fast_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reasoner_seen = Arc::new(std::sync::Mutex::new(Vec::new()));

        let cascade = ComputeCascade::empty()
            .with_fast(FlakyModel {
                tier: ModelTier::Fast,
                fail: false,
                content: "too vague".to_owned(),
                seen: Arc::clone(&fast_seen),
            })
            .with_reasoner(FlakyModel {
                tier: ModelTier::Reasoner,
                fail: false,
                content: "precise".to_owned(),
                seen: Arc::clone(&reasoner_seen),
            });

        // Reject the first answer with a concrete reason, accept the second.
        let outcome = cascade
            .run(
                &ModelRequest::new("summarize", ExpectedArtifact::Text)
                    .with_input(json!({ "task": "summarize" })),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![
                    VerificationVerdict::Retry {
                        reason: "missing the required detail".to_owned(),
                    },
                    VerificationVerdict::Sufficient,
                ]),
            )
            .await
            .unwrap();

        assert_eq!(outcome.response.content, "precise");

        // The reasoner saw the rejected artifact and why, not just the
        // original instruction replayed.
        let reasoner_input = reasoner_seen.lock().unwrap()[0].clone();
        assert_eq!(reasoner_input["task"], json!("summarize"));
        assert_eq!(
            reasoner_input["previous_attempt"]["rejected_artifact"],
            json!("too vague")
        );
        assert_eq!(
            reasoner_input["previous_attempt"]["verifier_reason"],
            json!("missing the required detail")
        );
        assert!(
            outcome
                .escalation_reasons
                .iter()
                .any(|r| r.contains("missing the required detail"))
        );
    }

    #[tokio::test]
    async fn transport_failure_is_recorded_as_an_attempt_and_escalates() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

        let cascade = ComputeCascade::empty()
            .with_fast(FlakyModel {
                tier: ModelTier::Fast,
                fail: true,
                content: String::new(),
                seen: Arc::clone(&seen),
            })
            .with_reasoner(FlakyModel {
                tier: ModelTier::Reasoner,
                fail: false,
                content: "recovered".to_owned(),
                seen: Arc::clone(&seen),
            });

        let outcome = cascade
            .run(
                &ModelRequest::new("task", ExpectedArtifact::Text),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![
                    VerificationVerdict::Sufficient,
                    VerificationVerdict::Sufficient,
                ]),
            )
            .await
            .unwrap();

        // The failed call is visible in the attempt record, not a gap.
        assert_eq!(outcome.attempts.len(), 2);
        assert_eq!(outcome.attempts[0].tier, ModelTier::Fast);
        assert!(outcome.attempts[0].identity.model.contains("flaky"));
        assert!(matches!(
            outcome.attempts[0].verdict,
            Some(VerificationVerdict::Retry { .. })
        ));
        assert_eq!(outcome.response.content, "recovered");

        // The escalation carried the transport failure forward.
        let reasoner_input = seen.lock().unwrap()[1].clone();
        assert!(
            reasoner_input["previous_attempt"]["failure"]
                .as_str()
                .unwrap()
                .contains("connection reset")
        );
    }

    #[tokio::test]
    async fn failed_tier_with_no_stronger_tier_reports_every_attempt() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cascade = ComputeCascade::empty().with_fast(FlakyModel {
            tier: ModelTier::Fast,
            fail: true,
            content: String::new(),
            seen: Arc::clone(&seen),
        });

        let err = cascade
            .run(
                &ModelRequest::new("task", ExpectedArtifact::Text),
                ModelTier::Fast,
                &ScriptedVerifier::new(vec![VerificationVerdict::Sufficient]),
            )
            .await
            .unwrap_err();

        match err {
            KnutError::ModelExhausted { attempts, reason } => {
                assert_eq!(attempts.len(), 1);
                assert!(reason.contains("connection reset"));
            }
            other => panic!("expected ModelExhausted, got {other:?}"),
        }
    }

    #[test]
    fn default_capabilities_do_not_overclaim() {
        // A plain adapter must not claim streaming/tools/reasoning.
        let caps = ModelCapabilities::buffered_text();
        assert!(!caps.streaming);
        assert!(!caps.tools);
        assert!(!caps.reasoning);
        assert!(!caps.usage);
    }

    #[tokio::test]
    async fn default_stream_wraps_a_buffered_adapter() {
        // Adapters that do not stream still satisfy the streaming path,
        // with an honest capability report.
        let model = ScriptedStreamer::new(vec![]);
        assert!(model.capabilities().streaming);

        let buffered = FakeModel::always_ok(ModelTier::Reasoner);
        assert!(!buffered.capabilities().streaming);
        let mut sink = BufferedSink::new();
        let response = buffered
            .stream(&ModelRequest::new("hi", ExpectedArtifact::Text), &mut sink)
            .await
            .unwrap();
        assert_eq!(response.content, "answer");
        assert!(matches!(
            sink.events().as_slice(),
            [ModelStreamEvent::TextDelta { text }, ModelStreamEvent::Completed { .. }]
                if text == "answer"
        ));
    }
}
