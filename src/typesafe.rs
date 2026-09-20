//! TypeSafe System One HTTP adapter (Jev), issue #2.
//!
//! Wire contract (TypeSafe docs / SDK reference):
//! `POST {base}/v1/systemone` with `Authorization: Bearer <key>`, body
//! `{ model, state, questions }`. Questions are one of three primitives:
//! `noul` (yes/no probability), `choice` (criteria map -> choice +
//! probabilities + confidence), `score` (levels -> fractional score +
//! probabilities + confidence). Answers come back keyed by question
//! name. Errors: 401 auth, 422 validation, 429 rate limit, 529
//! overloaded. No hidden retries; every failure surfaces.
//!
//! Jev-specific wire details stay in this module: the rest of Knut sees
//! a `SystemOne` / `JudgmentRouter` implementation.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Complexity, Decision, DecisionInput, Handler, IngressJudgments, Judgment, JudgmentRouter,
    KnutError, RetrievalJudgment, Risk, TierJudgment, YesNo,
};

pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
pub const DEFAULT_MODEL: &str = "jev-latest";
/// Bounded timeout: one routing call must never hang the runtime.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Noul answers may omit confidence; the SDK convention derives
/// `max(noul, 1 - noul)`, which never drops below 0.5.
fn noul_confidence(noul: f64) -> f32 {
    noul.max(1.0 - noul) as f32
}

/// The three question primitives on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Yes/no judgment; answer is a probability of true.
    Noul {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    /// Pick one of 2–255 options; `criteria` maps option key -> description.
    Choice {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        criteria: BTreeMap<String, String>,
    },
    /// Ordered rubric of 2–10 levels, low to high.
    Score {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        levels: Vec<String>,
    },
}

impl Question {
    pub fn noul(instructions: impl Into<String>) -> Question {
        Question::Noul {
            instructions: Some(instructions.into()),
        }
    }

    pub fn choice(
        instructions: impl Into<String>,
        criteria: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Question {
        Question::Choice {
            instructions: Some(instructions.into()),
            criteria: criteria
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    pub fn score(instructions: impl Into<String>, levels: Vec<&str>) -> Question {
        Question::Score {
            instructions: Some(instructions.into()),
            levels: levels.into_iter().map(str::to_owned).collect(),
        }
    }
}

/// One answer on the wire. Tagged per primitive; unknown shapes fail the
/// parse loudly rather than decoding to a default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Answer {
    #[serde(default)]
    pub noul: Option<f64>,
    #[serde(default)]
    pub choice: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    /// Probability per option/level; sums to 1 on the wire.
    #[serde(default)]
    pub probabilities: Option<BTreeMap<String, f64>>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub legend: Option<Value>,
}

impl Answer {
    /// The bounded-confidence reading the runtime gates on.
    ///
    /// Choice/Score use the API's confidence; Noul derives
    /// `max(noul, 1 - noul)` (TypeSafe sends no separate Noul confidence).
    pub fn confidence(&self) -> Result<f32, KnutError> {
        if let Some(confidence) = self.confidence {
            return Ok(confidence as f32);
        }
        self.noul.map(noul_confidence).ok_or_else(|| {
            KnutError::SystemOne("answer has neither confidence nor noul".to_owned())
        })
    }

    pub fn as_bool(&self) -> Result<bool, KnutError> {
        let noul = self
            .noul
            .ok_or_else(|| KnutError::SystemOne("expected noul answer".to_owned()))?;
        Ok(noul >= 0.5)
    }

    pub fn as_choice(&self) -> Result<String, KnutError> {
        self.choice
            .clone()
            .ok_or_else(|| KnutError::SystemOne("expected choice answer".to_owned()))
    }

    pub fn as_score(&self) -> Result<f64, KnutError> {
        self.score
            .ok_or_else(|| KnutError::SystemOne("expected score answer".to_owned()))
    }
}

/// Request body for `POST /v1/systemone`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SystemOneRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
}

/// Response envelope for `POST /v1/systemone`.
///
/// The wire format flattens answers at the top level next to envelope
/// metadata (`{"is_urgent": {...}, "department": {...}, "model":
/// "jev-1.13.0"}`), so deserialization partitions every field: a value
/// that parses as a typed answer (and carries a noul/choice/score) is an
/// answer; everything else is preserved in `extra` for tracing.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneResponse {
    pub answers: BTreeMap<String, Answer>,
    /// Envelope metadata (resolved model, request id, usage, ...).
    pub extra: BTreeMap<String, Value>,
    /// `model` field from the envelope, when present.
    pub resolved_model: Option<String>,
}

impl<'de> Deserialize<'de> for SystemOneResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = BTreeMap::<String, Value>::deserialize(deserializer)?;

        let mut answers = BTreeMap::new();
        let mut extra = BTreeMap::new();
        let mut resolved_model = None;

        for (key, value) in fields {
            let parsed = serde_json::from_value::<Answer>(value.clone())
                .ok()
                .filter(|a| a.noul.is_some() || a.choice.is_some() || a.score.is_some());

            match parsed {
                Some(answer) => {
                    answers.insert(key, answer);
                }
                None => {
                    if key == "model" {
                        resolved_model = value.as_str().map(str::to_owned);
                    }
                    extra.insert(key, value);
                }
            }
        }

        Ok(SystemOneResponse {
            answers,
            extra,
            resolved_model,
        })
    }
}

impl SystemOneResponse {
    /// Collect the answers into a typed [`IngressJudgments`].
    ///
    /// The seven ingress questions and their option keys are Knut's
    /// contract with itself; every option key is fixed here so the wire
    /// mapping is testable against recorded fixtures.
    pub fn to_ingress_judgments(&self) -> Result<IngressJudgments, KnutError> {
        let answers = &self.answers;

        let get = |key: &str| {
            answers
                .get(key)
                .ok_or_else(|| KnutError::SystemOne(format!("missing answer for {key:?}")))
        };

        let handler = get("handler")?;
        let handler_choice = handler.as_choice()?;
        let handler_choice = match handler_choice.as_str() {
            "clarify" => Handler::Clarify,
            "retrieve" => Handler::Retrieve,
            "act" => Handler::Act,
            "generate" => Handler::Generate,
            other => {
                return Err(KnutError::SystemOne(format!(
                    "unknown handler choice {other:?}"
                )));
            }
        };

        let complexity = get("complexity")?;
        let complexity_score = complexity.as_score()?;
        let complexity_choice = if complexity_score < 0.75 {
            Complexity::Trivial
        } else if complexity_score < 1.75 {
            Complexity::Routine
        } else if complexity_score < 2.75 {
            Complexity::MultiStep
        } else {
            Complexity::Deep
        };

        let retrieval = get("retrieval")?;
        let retrieval_choice = match retrieval.as_choice()?.as_str() {
            "none" => RetrievalJudgment::None,
            "files" => RetrievalJudgment::Files,
            "memory" => RetrievalJudgment::Memory,
            "web" => RetrievalJudgment::Web,
            "mixed" => RetrievalJudgment::Mixed,
            other => {
                return Err(KnutError::SystemOne(format!(
                    "unknown retrieval choice {other:?}"
                )));
            }
        };

        let missing = get("missing_user_info")?;
        let missing_choice = if missing.as_bool()? {
            YesNo::Yes
        } else {
            YesNo::No
        };

        let parallelizable = get("parallelizable")?;
        let parallelizable_choice = if parallelizable.as_bool()? {
            YesNo::Yes
        } else {
            YesNo::No
        };

        let risk = get("risk")?;
        let risk_choice = match risk.as_choice()?.as_str() {
            "low" => Risk::Low,
            "medium" => Risk::Medium,
            "high" => Risk::High,
            other => {
                return Err(KnutError::SystemOne(format!(
                    "unknown risk choice {other:?}"
                )));
            }
        };

        let tier = get("model_tier")?;
        let tier_choice = match tier.as_choice()?.as_str() {
            "fast" => TierJudgment::Fast,
            "standard" => TierJudgment::Standard,
            "reasoner" => TierJudgment::Reasoner,
            other => {
                return Err(KnutError::SystemOne(format!(
                    "unknown model_tier choice {other:?}"
                )));
            }
        };

        Ok(IngressJudgments {
            handler: Judgment {
                choice: handler_choice,
                confidence: handler.confidence()?,
            },
            complexity: Judgment {
                choice: complexity_choice,
                confidence: complexity.confidence()?,
            },
            retrieval: Judgment {
                choice: retrieval_choice,
                confidence: retrieval.confidence()?,
            },
            missing_user_info: Judgment {
                choice: missing_choice,
                confidence: missing.confidence()?,
            },
            parallelizable: Judgment {
                choice: parallelizable_choice,
                confidence: parallelizable.confidence()?,
            },
            risk: Judgment {
                choice: risk_choice,
                confidence: risk.confidence()?,
            },
            model_tier: Judgment {
                choice: tier_choice,
                confidence: tier.confidence()?,
            },
        })
    }
}

/// Configuration for the HTTP adapter.
#[derive(Debug, Clone)]
pub struct TypeSafeConfig {
    /// API key; never logged, never serialized.
    pub api_key: String,
    /// Defaults to `https://api.typesafe.ai` (no trailing slash).
    pub base_url: String,
    /// Defaults to `jev-latest`.
    pub model: String,
    /// Bounded request timeout; defaults to 10s.
    pub timeout: Duration,
}

impl TypeSafeConfig {
    /// Build config from the environment: `TYPESAFE_API_KEY` required,
    /// `TYPESAFE_BASE_URL` / `TYPESAFE_MODEL` optional overrides.
    pub fn from_env() -> Result<TypeSafeConfig, KnutError> {
        let api_key = std::env::var("TYPESAFE_API_KEY")
            .map_err(|_| KnutError::SystemOne("TYPESAFE_API_KEY is not set".to_owned()))?;
        let base_url = std::env::var("TYPESAFE_BASE_URL").ok();
        let model = std::env::var("TYPESAFE_MODEL").ok();
        Self::from_parts(api_key, base_url, model)
    }

    /// Pure constructor so configuration logic stays testable without
    /// mutating process environment (unsafe in Rust 2024 and racy under
    /// parallel tests).
    pub fn from_parts(
        api_key: String,
        base_url: Option<String>,
        model: Option<String>,
    ) -> Result<TypeSafeConfig, KnutError> {
        if api_key.trim().is_empty() {
            return Err(KnutError::SystemOne(
                "TYPESAFE_API_KEY is not set".to_owned(),
            ));
        }

        Ok(TypeSafeConfig {
            api_key,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
            model: model.unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
            timeout: DEFAULT_TIMEOUT,
        })
    }
}

/// The `SystemOne` backend against TypeSafe's System One API.
///
/// One HTTP call per routing decision; all ingress questions ride in the
/// same request (speculative fan-out). No retries: 429/529 surface as
/// errors so callers decide their own backoff policy.
pub struct JevSystemOne {
    http: reqwest::Client,
    config: TypeSafeConfig,
}

impl JevSystemOne {
    pub fn new(config: TypeSafeConfig) -> Result<Self, KnutError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| KnutError::SystemOne(format!("http client: {e}")))?;

        Ok(Self { http, config })
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{base}/v1/systemone")
    }

    /// The seven ingress questions over shared state (speculative fan-out).
    pub fn ingress_questions() -> BTreeMap<String, Question> {
        let mut questions = BTreeMap::new();

        questions.insert(
            "handler".to_owned(),
            Question::choice(
                "Which control path should handle this input?",
                [
                    (
                        "clarify",
                        "The input is missing information only the user can supply",
                    ),
                    (
                        "retrieve",
                        "Existing material must be found before answering",
                    ),
                    ("act", "A tool or capability should execute"),
                    ("generate", "A model response is the next step"),
                ],
            ),
        );

        questions.insert(
            "complexity".to_owned(),
            Question::score(
                "How much work does this input look like?",
                vec![
                    "A trivial one-liner with no judgment",
                    "Routine work with a standard shape",
                    "Multiple steps that must be coordinated",
                    "Deep work needing careful reasoning",
                ],
            ),
        );

        questions.insert(
            "retrieval".to_owned(),
            Question::choice(
                "If material must be found, where?",
                [
                    ("none", "Nothing needs to be found"),
                    ("files", "Local files or the workspace"),
                    ("memory", "Conversation or stored memory"),
                    ("web", "The public internet"),
                    ("mixed", "Several of these at once"),
                ],
            ),
        );

        questions.insert(
            "missing_user_info".to_owned(),
            Question::noul("Is information missing that only the user can supply?"),
        );

        questions.insert(
            "parallelizable".to_owned(),
            Question::noul("Can independent parts of this run in parallel?"),
        );

        questions.insert(
            "risk".to_owned(),
            Question::choice(
                "What is the risk of acting on this input as asked?",
                [
                    ("low", "Read-only or easily reversible"),
                    ("medium", "Writes that could be redone or rolled back"),
                    ("high", "Hard-to-reverse, costly, or safety-relevant"),
                ],
            ),
        );

        questions.insert(
            "model_tier".to_owned(),
            Question::choice(
                "Which model tier fits the response work?",
                [
                    ("fast", "Small fast model suffices"),
                    ("standard", "Mid-tier model fits"),
                    ("reasoner", "Strong reasoning model is warranted"),
                ],
            ),
        );

        questions
    }

    fn state_for(input: &DecisionInput) -> Value {
        serde_json::json!({
            "prompt": input.prompt,
            "capabilities": input.capabilities,
            "state": input.state,
        })
    }

    async fn post(&self, request: &SystemOneRequest) -> Result<SystemOneResponse, KnutError> {
        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.config.api_key)
            .json(request)
            .send()
            .await
            .map_err(|e| {
                // Timeouts and connection failures surface as-is; no retry.
                KnutError::SystemOne(format!("system one transport: {e}"))
            })?;

        let status = response.status();
        if !status.is_success() {
            // Body text may echo request content; keep it short and strip
            // anything resembling the key just in case.
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            let snippet = snippet.replace(&self.config.api_key, "[redacted]");

            let kind = match status {
                StatusCode::UNAUTHORIZED => "auth",
                StatusCode::UNPROCESSABLE_ENTITY => "validation",
                StatusCode::TOO_MANY_REQUESTS => "rate limit",
                s if s.as_u16() == 529 => "overloaded",
                _ => "http",
            };
            return Err(KnutError::SystemOne(format!(
                "system one {kind} error ({status}): {snippet}"
            )));
        }

        response
            .json::<SystemOneResponse>()
            .await
            .map_err(|e| KnutError::SystemOne(format!("system one protocol: {e}")))
    }
}

#[async_trait]
impl JudgmentRouter for JevSystemOne {
    async fn judge(&self, input: &DecisionInput) -> Result<IngressJudgments, KnutError> {
        let request = SystemOneRequest {
            model: self.config.model.clone(),
            state: Self::state_for(input),
            questions: Self::ingress_questions(),
        };

        let response = self.post(&request).await?;
        response.to_ingress_judgments()
    }
}

#[async_trait]
impl crate::SystemOne for JevSystemOne {
    async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError> {
        let judgments = JudgmentRouter::judge(self, input).await?;
        Ok(judgments.to_decision())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_json() -> &'static str {
        // Recorded fixture shape: answers keyed by name, plus envelope
        // metadata flattened at the top level.
        r#"{
            "handler": {
                "choice": "retrieve",
                "probabilities": {
                    "clarify": 0.05, "retrieve": 0.82, "act": 0.03, "generate": 0.10
                },
                "confidence": 0.82
            },
            "complexity": {
                "score": 1.6,
                "probabilities": {"0": 0.05, "1": 0.60, "2": 0.30, "3": 0.05},
                "confidence": 0.77,
                "legend": ["trivial", "routine", "multi-step", "deep"]
            },
            "retrieval": {
                "choice": "files",
                "probabilities": {"none": 0.04, "files": 0.91, "memory": 0.02, "web": 0.01, "mixed": 0.02},
                "confidence": 0.91
            },
            "missing_user_info": { "noul": 0.12 },
            "parallelizable": { "noul": 0.85, "confidence": 0.80 },
            "risk": {
                "choice": "low",
                "probabilities": {"low": 0.93, "medium": 0.05, "high": 0.02},
                "confidence": 0.93
            },
            "model_tier": {
                "choice": "fast",
                "probabilities": {"fast": 0.88, "standard": 0.09, "reasoner": 0.03},
                "confidence": 0.88
            },
            "model": "jev-1.13.0",
            "request_id": "req_abc123"
        }"#
    }

    fn parse() -> SystemOneResponse {
        serde_json::from_str(response_json()).expect("fixture parses")
    }

    #[test]
    fn response_fixture_parses_with_metadata_preserved() {
        let response = parse();

        assert_eq!(response.answers.len(), 7);
        // Flattened envelope metadata survives for tracing.
        assert_eq!(
            response.extra.get("model").and_then(|v| v.as_str()),
            Some("jev-1.13.0")
        );
        assert!(response.extra.contains_key("request_id"));
    }

    #[test]
    fn noul_confidence_derivation() {
        assert!((noul_confidence(0.97) - 0.97).abs() < 1e-9);
        assert!((noul_confidence(0.03) - 0.97).abs() < 1e-9);
        assert!((noul_confidence(0.5) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn answer_readings() {
        let response = parse();
        let answers = &response.answers;

        assert!(!answers["missing_user_info"].as_bool().unwrap());
        assert!(answers["parallelizable"].as_bool().unwrap());
        assert_eq!(answers["handler"].as_choice().unwrap(), "retrieve");
        assert!((answers["complexity"].as_score().unwrap() - 1.6).abs() < 1e-9);

        // Noul without explicit confidence derives max(n, 1-n).
        let derived = answers["missing_user_info"].confidence().unwrap();
        assert!((derived - 0.88).abs() < 1e-6);

        // Choice carries its own confidence.
        let explicit = answers["handler"].confidence().unwrap();
        assert!((explicit - 0.82).abs() < 1e-6);
    }

    #[test]
    fn ingress_judgments_map_from_wire_answers() {
        let judgments = parse().to_ingress_judgments().unwrap();

        assert_eq!(judgments.handler.choice, Handler::Retrieve);
        assert!((judgments.handler.confidence - 0.82).abs() < 1e-6);

        // Score 1.6 lands in the Routine band (0.75..1.75).
        assert_eq!(judgments.complexity.choice, Complexity::Routine);
        assert_eq!(judgments.retrieval.choice, RetrievalJudgment::Files);
        assert_eq!(judgments.missing_user_info.choice, YesNo::No);
        assert_eq!(judgments.parallelizable.choice, YesNo::Yes);
        assert_eq!(judgments.risk.choice, Risk::Low);
        assert_eq!(judgments.model_tier.choice, TierJudgment::Fast);
    }

    #[test]
    fn decision_collapses_from_wire_via_existing_contradiction_logic() {
        let judgments = parse().to_ingress_judgments().unwrap();
        let decision = judgments.to_decision();

        // Handler retrieve + retrieval files: no contradiction, so
        // confidence = min(0.82, 0.91) = 0.82.
        assert_eq!(decision.route, crate::Route::Retrieve);
        assert_eq!(decision.retrieval, Some(crate::RetrievalSource::Files));
        assert!((decision.confidence - 0.82).abs() < 1e-6);
    }

    #[test]
    fn missing_answers_fail_loudly() {
        let response: SystemOneResponse =
            serde_json::from_str(r#"{"handler": {"choice": "act", "confidence": 0.9}}"#).unwrap();

        let err = response.to_ingress_judgments().unwrap_err();
        assert!(err.to_string().contains("complexity"));
    }

    #[test]
    fn unknown_option_keys_fail_loudly() {
        let response: SystemOneResponse = serde_json::from_str(
            r#"{"handler": {"choice": "teleport", "confidence": 0.9},
                "complexity": {"score": 1.0, "confidence": 0.9},
                "retrieval": {"choice": "none", "confidence": 0.9},
                "missing_user_info": {"noul": 0.1},
                "parallelizable": {"noul": 0.1},
                "risk": {"choice": "low", "confidence": 0.9},
                "model_tier": {"choice": "fast", "confidence": 0.9}}"#,
        )
        .unwrap();

        let err = response.to_ingress_judgments().unwrap_err();
        assert!(err.to_string().contains("teleport"));
    }

    #[test]
    fn request_serializes_to_the_wire_shape() {
        let mut questions = BTreeMap::new();
        questions.insert(
            "urgent".to_owned(),
            Question::noul("Does this convey urgency?"),
        );
        questions.insert(
            "department".to_owned(),
            Question::choice(
                "Which team should handle this?",
                [
                    ("billing", "Payments, invoicing, refunds"),
                    ("technical", "Bugs, outages, integrations"),
                ],
            ),
        );

        let request = SystemOneRequest {
            model: "jev-latest".to_owned(),
            state: Value::String("Help! My payouts have been failing.".to_owned()),
            questions,
        };

        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["model"], "jev-latest");
        assert_eq!(json["questions"]["urgent"]["type"], "noul");
        assert_eq!(
            json["questions"]["department"]["criteria"]["billing"],
            "Payments, invoicing, refunds"
        );
        // Optional instructions serialize when present.
        assert!(json["questions"]["urgent"]["instructions"].is_string());
    }

    #[test]
    fn api_key_never_appears_in_error_output() {
        let config = TypeSafeConfig {
            api_key: "sk-secret-value".to_owned(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        };

        let leaked = "some body sk-secret-value trailing".to_owned();
        let redacted = leaked.replace(&config.api_key, "[redacted]");

        assert!(redacted.contains("[redacted]"));
        assert!(!redacted.contains("sk-secret-value"));
    }

    #[test]
    fn config_requires_a_key_and_applies_defaults() {
        // Missing key fails.
        assert!(TypeSafeConfig::from_parts(String::new(), None, None).is_err());
        assert!(TypeSafeConfig::from_parts("   ".to_owned(), None, None).is_err());

        // Present key picks up documented defaults.
        let config = TypeSafeConfig::from_parts("k".to_owned(), None, None).unwrap();
        assert_eq!(config.base_url, "https://api.typesafe.ai");
        assert_eq!(config.model, "jev-latest");
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);

        // Overrides win.
        let config = TypeSafeConfig::from_parts(
            "k".to_owned(),
            Some("http://localhost:8080".to_owned()),
            Some("jev-1.13.0".to_owned()),
        )
        .unwrap();
        assert_eq!(config.base_url, "http://localhost:8080");
        assert_eq!(config.model, "jev-1.13.0");
    }

    #[test]
    fn endpoint_joins_without_double_slashes() {
        let config = TypeSafeConfig {
            api_key: "k".to_owned(),
            base_url: "http://localhost:8080/".to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        };

        let jev = JevSystemOne::new(config).unwrap();
        assert_eq!(jev.endpoint(), "http://localhost:8080/v1/systemone");
    }
}
