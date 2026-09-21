use std::time::Instant;

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

/// Token accounting for one call. `Default` is all-zero, which callers
/// must treat as "unknown", never as "free".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A bounded generative task.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    pub instruction: String,
    pub expected_artifact: ExpectedArtifact,
    /// Structured input/context for the task.
    pub input: Value,
}

impl ModelRequest {
    pub fn new(instruction: impl Into<String>, expected_artifact: ExpectedArtifact) -> Self {
        Self {
            instruction: instruction.into(),
            expected_artifact,
            input: Value::Null,
        }
    }

    pub fn with_input(mut self, input: Value) -> Self {
        self.input = input;
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
}

/// A provider adapter for one capability tier.
///
/// Implementations are configuration: the runtime never names providers.
#[async_trait]
pub trait Model: Send + Sync {
    fn identity(&self) -> ModelIdentity;

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError>;
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

    fn next_tier(tier: ModelTier) -> Option<ModelTier> {
        match tier {
            ModelTier::Fast => Some(ModelTier::Standard),
            ModelTier::Standard => Some(ModelTier::Reasoner),
            ModelTier::Reasoner => None,
        }
    }

    /// Run the bounded task starting at `starting_tier`, escalating until
    /// verification passes or the ladder runs out.
    pub async fn run(
        &self,
        request: &ModelRequest,
        starting_tier: ModelTier,
        verifier: &dyn Verifier,
    ) -> Result<CascadeOutcome, KnutError> {
        let mut attempts = Vec::new();
        let mut escalation_reasons = Vec::new();
        let mut tier = starting_tier;

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
                        return Err(KnutError::ModelExhausted {
                            reason: "no model configured for any tier".to_owned(),
                            attempts,
                        });
                    }
                }
            };

            let started = Instant::now();
            let response = model.complete(request).await?;
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
                        escalation_reasons.push(reason.clone());
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

            Ok(ModelResponse {
                content,
                identity: self.identity.clone(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
                latency: std::time::Duration::from_millis(1),
            })
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

        assert_eq!(
            outcome.response.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 5,
            }
        );
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
                Ok(ModelResponse {
                    content: "ok".to_owned(),
                    identity: self.identity(),
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                    latency: std::time::Duration::ZERO,
                })
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
}
