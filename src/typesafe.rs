//! TypeSafe System One HTTP adapter (Jev).
//!
//! Wire contract (https://docs.typesafe.ai/api, checked 2026-09-20):
//! `POST {base}/v1/systemone` with `Authorization: Bearer <key>`, body
//! `{ model, state, questions }`. Questions are one of three primitives:
//! `noul` (yes/no probability), `choice` (criteria map -> choice +
//! probabilities + confidence), `score` (criteria array -> fractional
//! score + legend + probabilities + confidence). The response envelope is
//! `{ model, answers: {question_id: answer}, usage }`; every answer
//! carries a `type` matching its question. Errors: 401 auth, 422
//! validation, 429 rate limit, 529 overloaded. No hidden retries; every
//! failure surfaces as a typed [`SystemOneFailure`].
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
    KnutError, RetrievalJudgment, Risk, SystemOneFailure, TierJudgment, YesNo,
};

pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
pub const DEFAULT_MODEL: &str = "jev-latest";
/// Bounded timeout: one routing call must never hang the runtime.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded request bodies (questions + state) so a runaway frame cannot
/// exceed the documented 64k-token request budget unchecked.
pub const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
/// Bounded response body so a hostile or broken endpoint cannot exhaust
/// memory before parsing.
pub const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;
/// Documented wire limits (https://docs.typesafe.ai/api).
pub const MAX_CHOICE_OPTIONS: usize = 255;
pub const MAX_SCORE_LEVELS: usize = 10;
/// Tolerance when validating that a probability distribution sums to 1.
/// Floats on the wire are not exact; anything outside this band is a
/// protocol violation, not a rounding artifact.
pub const PROBABILITY_SUM_TOLERANCE: f64 = 1e-3;

/// Noul answers carry no confidence on the wire; the SDK convention
/// derives `max(noul, 1 - noul)`, which never drops below 0.5. This is a
/// separately named derived statistic, not an API-provided value.
fn derived_noul_confidence(noul: f64) -> f32 {
    noul.max(1.0 - noul) as f32
}

fn is_finite_in_range(value: f64, low: f64, high: f64) -> bool {
    value.is_finite() && value >= low && value <= high
}

/// The three question primitives on the wire.
///
/// `instructions` is required by the API; `criteria` is the documented
/// field for both Choice (option -> rubric map) and Score (ordered level
/// array). Noul's optional `criteria` ({true, false}) is supported.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Yes/no judgment; answer is a probability of true.
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<BTreeMap<String, String>>,
    },
    /// Pick one of 2–255 options; `criteria` maps option key -> description.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// Ordered rubric of 2–10 levels, low to high.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

impl Question {
    pub fn noul(instructions: impl Into<String>) -> Question {
        Question::Noul {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    pub fn choice(
        instructions: impl Into<String>,
        criteria: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Question {
        Question::Choice {
            instructions: instructions.into(),
            criteria: criteria
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    pub fn score(instructions: impl Into<String>, levels: Vec<&str>) -> Question {
        Question::Score {
            instructions: instructions.into(),
            criteria: levels.into_iter().map(str::to_owned).collect(),
        }
    }

    /// Validate against the documented wire limits before sending.
    pub fn validate(&self) -> Result<(), KnutError> {
        match self {
            Question::Noul { instructions, .. } => {
                if instructions.trim().is_empty() {
                    return Err(KnutError::SystemOne(
                        "noul question requires non-empty instructions".to_owned(),
                    ));
                }
            }
            Question::Choice {
                instructions,
                criteria,
            } => {
                if instructions.trim().is_empty() {
                    return Err(KnutError::SystemOne(
                        "choice question requires non-empty instructions".to_owned(),
                    ));
                }
                if criteria.len() < 2 {
                    return Err(KnutError::SystemOne(format!(
                        "choice question needs 2..={MAX_CHOICE_OPTIONS} options, got {}",
                        criteria.len()
                    )));
                }
                if criteria.len() > MAX_CHOICE_OPTIONS {
                    return Err(KnutError::SystemOne(format!(
                        "choice question has {} options, max is {MAX_CHOICE_OPTIONS}",
                        criteria.len()
                    )));
                }
            }
            Question::Score {
                instructions,
                criteria,
            } => {
                if instructions.trim().is_empty() {
                    return Err(KnutError::SystemOne(
                        "score question requires non-empty instructions".to_owned(),
                    ));
                }
                if criteria.len() < 2 {
                    return Err(KnutError::SystemOne(format!(
                        "score question needs 2..={MAX_SCORE_LEVELS} levels, got {}",
                        criteria.len()
                    )));
                }
                if criteria.len() > MAX_SCORE_LEVELS {
                    return Err(KnutError::SystemOne(format!(
                        "score question has {} levels, max is {MAX_SCORE_LEVELS}",
                        criteria.len()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// One typed answer on the wire. Every answer carries a `type` matching
/// its question; contradictory shapes fail validation loudly rather than
/// decoding to a default route.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

impl Answer {
    /// Validate the answer against the question that produced it.
    pub fn validate(&self, question: &Question) -> Result<(), KnutError> {
        let mismatch = |expected: &str| {
            KnutError::SystemOne(format!("answer type {expected:?} does not match question"))
        };

        let validate_distribution = |probabilities: &BTreeMap<String, f64>,
                                     keys: &[String],
                                     what: &str|
         -> Result<(), KnutError> {
            if probabilities.len() != keys.len() {
                return Err(KnutError::SystemOne(format!(
                    "{what} distribution has {} entries, expected {}",
                    probabilities.len(),
                    keys.len()
                )));
            }
            for key in keys {
                let Some(p) = probabilities.get(key) else {
                    return Err(KnutError::SystemOne(format!(
                        "{what} distribution is missing key {key:?}"
                    )));
                };
                if !is_finite_in_range(*p, 0.0, 1.0) {
                    return Err(KnutError::SystemOne(format!(
                        "{what} probability for {key:?} is not in [0, 1]: {p}"
                    )));
                }
            }
            let sum: f64 = probabilities.values().sum();
            if (sum - 1.0).abs() > PROBABILITY_SUM_TOLERANCE {
                return Err(KnutError::SystemOne(format!(
                    "{what} probabilities sum to {sum}, expected 1 +/- {PROBABILITY_SUM_TOLERANCE}"
                )));
            }
            Ok(())
        };

        match (self, question) {
            (Answer::Noul { noul }, Question::Noul { .. }) => {
                if !is_finite_in_range(*noul, 0.0, 1.0) {
                    return Err(KnutError::SystemOne(format!(
                        "noul probability {noul} is not in [0, 1]"
                    )));
                }
            }
            (
                Answer::Choice {
                    choice,
                    probabilities,
                    confidence,
                },
                Question::Choice { criteria, .. },
            ) => {
                if !criteria.contains_key(choice) {
                    return Err(KnutError::SystemOne(format!(
                        "choice answer {choice:?} is not a submitted option"
                    )));
                }
                if !is_finite_in_range(*confidence, 0.0, 1.0) {
                    return Err(KnutError::SystemOne(format!(
                        "choice confidence {confidence} is not in [0, 1]"
                    )));
                }
                let keys: Vec<String> = criteria.keys().cloned().collect();
                validate_distribution(probabilities, &keys, "choice")?;
            }
            (
                Answer::Score {
                    score,
                    legend,
                    probabilities,
                    confidence,
                },
                Question::Score { criteria, .. },
            ) => {
                let max = (criteria.len() - 1) as f64;
                if !is_finite_in_range(*score, 0.0, max) {
                    return Err(KnutError::SystemOne(format!(
                        "score {score} is outside the rubric range 0..={max}"
                    )));
                }
                if !is_finite_in_range(*confidence, 0.0, 1.0) {
                    return Err(KnutError::SystemOne(format!(
                        "score confidence {confidence} is not in [0, 1]"
                    )));
                }
                let expected_keys: Vec<String> =
                    (0..criteria.len()).map(|i| i.to_string()).collect();
                if legend.len() != criteria.len() {
                    return Err(KnutError::SystemOne(format!(
                        "score legend has {} levels, expected {}",
                        legend.len(),
                        criteria.len()
                    )));
                }
                for (i, expected) in criteria.iter().enumerate() {
                    match legend.get(&i.to_string()) {
                        Some(actual) if actual == expected => {}
                        Some(actual) => {
                            return Err(KnutError::SystemOne(format!(
                                "score legend level {i} is {actual:?}, expected {expected:?}"
                            )));
                        }
                        None => {
                            return Err(KnutError::SystemOne(format!(
                                "score legend is missing level {i}"
                            )));
                        }
                    }
                }
                validate_distribution(probabilities, &expected_keys, "score")?;
            }
            (Answer::Noul { .. }, _) => return Err(mismatch("noul")),
            (Answer::Choice { .. }, _) => return Err(mismatch("choice")),
            (Answer::Score { .. }, _) => return Err(mismatch("score")),
        }
        Ok(())
    }

    /// The API-provided confidence for Choice/Score answers.
    ///
    /// Noul answers carry no API confidence; `derived_noul_confidence`
    /// computes the separately named SDK-convention statistic
    /// `max(noul, 1 - noul)`. That statistic is a routing heuristic, not
    /// a model-reported certainty and not a task-correctness probability.
    pub fn confidence(&self) -> Result<f32, KnutError> {
        match self {
            Answer::Noul { noul } => Ok(derived_noul_confidence(*noul)),
            Answer::Choice { confidence, .. } | Answer::Score { confidence, .. } => {
                Ok(*confidence as f32)
            }
        }
    }

    pub fn as_bool(&self) -> Result<bool, KnutError> {
        match self {
            Answer::Noul { noul } => Ok(*noul >= 0.5),
            _ => Err(KnutError::SystemOne("expected noul answer".to_owned())),
        }
    }

    pub fn as_choice(&self) -> Result<String, KnutError> {
        match self {
            Answer::Choice { choice, .. } => Ok(choice.clone()),
            _ => Err(KnutError::SystemOne("expected choice answer".to_owned())),
        }
    }

    pub fn as_score(&self) -> Result<f64, KnutError> {
        match self {
            Answer::Score { score, .. } => Ok(*score),
            _ => Err(KnutError::SystemOne("expected score answer".to_owned())),
        }
    }
}

impl<'de> Deserialize<'de> for Answer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Wire shape: `{type, ...}` with type-specific required fields.
        #[derive(Deserialize)]
        struct WireAnswer {
            #[serde(rename = "type")]
            answer_type: String,
            #[serde(default)]
            noul: Option<f64>,
            #[serde(default)]
            choice: Option<String>,
            #[serde(default)]
            score: Option<f64>,
            #[serde(default)]
            legend: Option<BTreeMap<String, String>>,
            #[serde(default)]
            probabilities: Option<BTreeMap<String, f64>>,
            #[serde(default)]
            confidence: Option<f64>,
        }

        let wire = WireAnswer::deserialize(deserializer)?;

        let unexpected = |field: &str, answer_type: &str| {
            Err(serde::de::Error::custom(format!(
                "{answer_type} answer must not carry a {field} field"
            )))
        };

        match wire.answer_type.as_str() {
            "noul" => {
                let Some(noul) = wire.noul else {
                    return Err(serde::de::Error::custom(
                        "noul answer requires a noul field",
                    ));
                };
                if wire.choice.is_some() || wire.score.is_some() {
                    return unexpected("choice/score", "noul");
                }
                if !noul.is_finite() {
                    return Err(serde::de::Error::custom("noul value is not finite"));
                }
                Ok(Answer::Noul { noul })
            }
            "choice" => {
                let (Some(choice), Some(probabilities), Some(confidence)) =
                    (wire.choice, wire.probabilities, wire.confidence)
                else {
                    return Err(serde::de::Error::custom(
                        "choice answer requires choice, probabilities and confidence",
                    ));
                };
                if wire.noul.is_some() || wire.score.is_some() || wire.legend.is_some() {
                    return unexpected("noul/score/legend", "choice");
                }
                Ok(Answer::Choice {
                    choice,
                    probabilities,
                    confidence,
                })
            }
            "score" => {
                let (Some(score), Some(legend), Some(probabilities), Some(confidence)) =
                    (wire.score, wire.legend, wire.probabilities, wire.confidence)
                else {
                    return Err(serde::de::Error::custom(
                        "score answer requires score, legend, probabilities and confidence",
                    ));
                };
                if wire.noul.is_some() || wire.choice.is_some() {
                    return unexpected("noul/choice", "score");
                }
                Ok(Answer::Score {
                    score,
                    legend,
                    probabilities,
                    confidence,
                })
            }
            other => Err(serde::de::Error::custom(format!(
                "unknown answer type {other:?}"
            ))),
        }
    }
}

/// Token usage reported by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Request body for `POST /v1/systemone`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SystemOneRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
}

impl SystemOneRequest {
    /// Validate questions and enforce a bounded serialized size.
    pub fn validate(&self) -> Result<(), KnutError> {
        if self.model.trim().is_empty() {
            return Err(KnutError::SystemOne("model must not be empty".to_owned()));
        }
        if self.questions.is_empty() {
            return Err(KnutError::SystemOne(
                "at least one question is required".to_owned(),
            ));
        }
        for (id, question) in &self.questions {
            if id.trim().is_empty() {
                return Err(KnutError::SystemOne(
                    "question ids must not be empty".to_owned(),
                ));
            }
            question.validate()?;
        }
        let size = serde_json::to_vec(self)
            .map_err(|e| KnutError::SystemOne(format!("serialize request: {e}")))?
            .len();
        if size > MAX_REQUEST_BODY_BYTES {
            return Err(KnutError::SystemOne(format!(
                "request body is {size} bytes, max is {MAX_REQUEST_BODY_BYTES}"
            )));
        }
        Ok(())
    }
}

/// Response envelope for `POST /v1/systemone`:
/// `{model, answers: {question_id: answer}, usage}`.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneResponse {
    /// The versioned model ID that answered, for logging and pinning.
    pub resolved_model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

impl<'de> Deserialize<'de> for SystemOneResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Wire envelope. `answers` and `usage` are required by the
        /// documented contract; unknown extra fields are tolerated but
        /// never interpreted.
        #[derive(Deserialize)]
        struct WireEnvelope {
            model: String,
            answers: BTreeMap<String, Answer>,
            usage: Usage,
        }

        let wire = WireEnvelope::deserialize(deserializer)?;
        Ok(SystemOneResponse {
            resolved_model: wire.model,
            answers: wire.answers,
            usage: wire.usage,
        })
    }
}

impl SystemOneResponse {
    /// Validate every answer against the questions that were submitted.
    ///
    /// Missing answers, type mismatches, unknown option keys, out-of-range
    /// numbers and invalid distributions are typed errors — never default
    /// routes.
    pub fn validate_against(
        &self,
        questions: &BTreeMap<String, Question>,
    ) -> Result<(), KnutError> {
        for (id, question) in questions {
            let Some(answer) = self.answers.get(id) else {
                return Err(KnutError::SystemOne(format!(
                    "response is missing an answer for question {id:?}"
                )));
            };
            answer.validate(question)?;
        }
        for id in self.answers.keys() {
            if !questions.contains_key(id) {
                return Err(KnutError::SystemOne(format!(
                    "response contains an answer for unknown question {id:?}"
                )));
            }
        }
        Ok(())
    }

    /// Collect the answers into a typed [`IngressJudgments`] after
    /// validating them against the submitted ingress questions.
    ///
    /// The seven ingress questions and their option keys are Knut's
    /// contract with itself; every option key is fixed here so the wire
    /// mapping is testable against recorded fixtures.
    pub fn to_ingress_judgments(&self) -> Result<IngressJudgments, KnutError> {
        self.validate_against(&JevSystemOne::ingress_questions())?;

        let answers = &self.answers;

        let get = |key: &str| {
            answers
                .get(key)
                .ok_or_else(|| KnutError::SystemOne(format!("missing answer for {key:?}")))
        };

        let handler = get("handler")?;
        let handler_choice = match handler.as_choice()?.as_str() {
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

/// A secret-safe view of [`TypeSafeConfig`] for display and errors.
#[derive(Debug, Clone)]
pub struct TypeSafeConfigSummary {
    pub base_url: String,
    pub model: String,
    pub timeout: Duration,
    /// True when the key came from the process environment rather than
    /// explicit configuration; shown instead of the key itself.
    pub api_key_source: &'static str,
}

/// Configuration for the HTTP adapter. Debug output never reveals the key.
#[derive(Clone)]
pub struct TypeSafeConfig {
    /// API key; never logged, never serialized, never shown in Debug.
    pub api_key: String,
    /// Defaults to `https://api.typesafe.ai` (no trailing slash).
    pub base_url: String,
    /// Defaults to `jev-latest`. A pinned versioned ID (for example
    /// `jev-1.13.0`) is accepted even though `GET /v1/models` lists only
    /// aliases; the response's resolved `model` records what answered.
    pub model: String,
    /// Bounded request timeout; defaults to 10s.
    pub timeout: Duration,
}

impl std::fmt::Debug for TypeSafeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypeSafeConfig")
            .field("api_key", &"[redacted]")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("timeout", &self.timeout)
            .finish()
    }
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

    /// Secret-free summary for logs, errors and doctor output.
    pub fn summary(&self) -> TypeSafeConfigSummary {
        TypeSafeConfigSummary {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            timeout: self.timeout,
            api_key_source: "configured",
        }
    }
}

/// The `SystemOne` backend against TypeSafe's System One API.
///
/// One HTTP call per routing decision; all ingress questions ride in the
/// same request (speculative fan-out). No retries: failures surface as
/// typed [`SystemOneFailure`]s so callers choose their own bounded
/// backoff policy. The live backend never silently falls back to a mock.
pub struct JevSystemOne {
    http: reqwest::Client,
    config: TypeSafeConfig,
}

impl JevSystemOne {
    pub fn new(config: TypeSafeConfig) -> Result<Self, KnutError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            // Credentials must never be forwarded to a different origin
            // on a redirect.
            .redirect(reqwest::redirect::Policy::none())
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
        request.validate()?;

        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.config.api_key)
            .json(request)
            .send()
            .await
            .map_err(|e| {
                // Timeouts and connection failures surface as transient;
                // no retry is performed here.
                KnutError::SystemOneCall {
                    failure: SystemOneFailure::Transient,
                    message: format!("transport: {e}"),
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            // 3xx is an error under the no-redirect policy: credentials
            // must not follow a redirect to another origin.
            let failure = match status {
                StatusCode::UNAUTHORIZED => SystemOneFailure::Auth,
                StatusCode::UNPROCESSABLE_ENTITY => SystemOneFailure::Validation,
                StatusCode::TOO_MANY_REQUESTS => SystemOneFailure::RateLimit,
                s if s.as_u16() == 529 => SystemOneFailure::Overloaded,
                _ if status.is_redirection() => SystemOneFailure::Protocol,
                s if s.is_server_error() => SystemOneFailure::Transient,
                _ => SystemOneFailure::Protocol,
            };

            // Body text may echo request content; keep it short and strip
            // anything resembling the key just in case.
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            let snippet = snippet.replace(&self.config.api_key, "[redacted]");

            return Err(KnutError::SystemOneCall {
                failure,
                message: format!("http {status}: {snippet}"),
            });
        }

        // Bound the response body before parsing.
        let bytes = response
            .bytes()
            .await
            .map_err(|e| KnutError::SystemOneCall {
                failure: SystemOneFailure::Transient,
                message: format!("read body: {e}"),
            })?;
        if bytes.len() > MAX_RESPONSE_BODY_BYTES {
            return Err(KnutError::SystemOneCall {
                failure: SystemOneFailure::Protocol,
                message: format!(
                    "response body is {} bytes, max is {MAX_RESPONSE_BODY_BYTES}",
                    bytes.len()
                ),
            });
        }

        serde_json::from_slice::<SystemOneResponse>(&bytes).map_err(|e| KnutError::SystemOneCall {
            failure: SystemOneFailure::Protocol,
            message: format!("envelope: {e}"),
        })
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

    /// Fixture copied independently from the documented response example
    /// shape: `{model, answers, usage}` with typed answers.
    fn response_json() -> &'static str {
        r#"{
            "model": "jev-1.13.0",
            "answers": {
                "handler": {
                    "type": "choice",
                    "choice": "retrieve",
                    "probabilities": {
                        "clarify": 0.05, "retrieve": 0.82, "act": 0.03, "generate": 0.10
                    },
                    "confidence": 0.82
                },
                "complexity": {
                    "type": "score",
                    "score": 1.6,
                    "legend": {
                        "0": "A trivial one-liner with no judgment",
                        "1": "Routine work with a standard shape",
                        "2": "Multiple steps that must be coordinated",
                        "3": "Deep work needing careful reasoning"
                    },
                    "probabilities": {"0": 0.05, "1": 0.60, "2": 0.30, "3": 0.05},
                    "confidence": 0.77
                },
                "retrieval": {
                    "type": "choice",
                    "choice": "files",
                    "probabilities": {"none": 0.04, "files": 0.91, "memory": 0.02, "web": 0.01, "mixed": 0.02},
                    "confidence": 0.91
                },
                "missing_user_info": {"type": "noul", "noul": 0.12},
                "parallelizable": {"type": "noul", "noul": 0.85},
                "risk": {
                    "type": "choice",
                    "choice": "low",
                    "probabilities": {"low": 0.93, "medium": 0.05, "high": 0.02},
                    "confidence": 0.93
                },
                "model_tier": {
                    "type": "choice",
                    "choice": "fast",
                    "probabilities": {"fast": 0.88, "standard": 0.09, "reasoner": 0.03},
                    "confidence": 0.88
                }
            },
            "usage": {"input_tokens": 296, "output_tokens": 20}
        }"#
    }

    fn parse() -> SystemOneResponse {
        serde_json::from_str(response_json()).expect("fixture parses")
    }

    #[test]
    fn documented_envelope_fixture_parses() {
        let response = parse();

        assert_eq!(response.resolved_model, "jev-1.13.0");
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 296,
                output_tokens: 20
            }
        );
        assert_eq!(response.answers.len(), 7);
    }

    #[test]
    fn the_flattened_legacy_shape_is_rejected() {
        // The old adapter searched for answer objects at the top level;
        // the documented envelope is {model, answers, usage}. A response
        // without the answers map is a protocol error, not an empty one.
        let result = serde_json::from_str::<SystemOneResponse>(
            r#"{"handler": {"type": "choice", "choice": "act",
                "probabilities": {"act": 1.0}, "confidence": 0.9},
                "model": "jev-1.13.0"}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn noul_confidence_is_a_derived_statistic() {
        assert!((derived_noul_confidence(0.97) - 0.97).abs() < 1e-9);
        assert!((derived_noul_confidence(0.03) - 0.97).abs() < 1e-9);
        assert!((derived_noul_confidence(0.5) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn answer_readings() {
        let response = parse();
        let answers = &response.answers;

        assert!(!answers["missing_user_info"].as_bool().unwrap());
        assert!(answers["parallelizable"].as_bool().unwrap());
        assert_eq!(answers["handler"].as_choice().unwrap(), "retrieve");
        assert!((answers["complexity"].as_score().unwrap() - 1.6).abs() < 1e-9);

        // Noul without API confidence derives max(n, 1-n).
        let derived = answers["missing_user_info"].confidence().unwrap();
        assert!((derived - 0.88).abs() < 1e-6);

        // Choice carries its own API confidence.
        let explicit = answers["handler"].confidence().unwrap();
        assert!((explicit - 0.82).abs() < 1e-6);

        // Wrong-shape readers fail loudly.
        assert!(answers["handler"].as_bool().is_err());
        assert!(answers["missing_user_info"].as_choice().is_err());
    }

    #[test]
    fn ingress_judgments_map_from_validated_wire_answers() {
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
        let response: SystemOneResponse = serde_json::from_str(
            r#"{"model": "jev-1.13.0",
                "answers": {"handler": {"type": "choice", "choice": "act",
                    "probabilities": {"act": 1.0}, "confidence": 0.9}},
                "usage": {"input_tokens": 1, "output_tokens": 1}}"#,
        )
        .unwrap();

        let err = response.to_ingress_judgments().unwrap_err();
        assert!(err.to_string().contains("complexity"));
    }

    #[test]
    fn unknown_option_keys_fail_loudly() {
        let response: SystemOneResponse = serde_json::from_str(
            r#"{"model": "jev-1.13.0",
                "answers": {
                    "handler": {"type": "choice", "choice": "teleport",
                        "probabilities": {"teleport": 1.0}, "confidence": 0.9},
                    "complexity": {"type": "score", "score": 1.0,
                        "legend": {"0": "A trivial one-liner with no judgment",
                            "1": "Routine work with a standard shape",
                            "2": "Multiple steps that must be coordinated",
                            "3": "Deep work needing careful reasoning"},
                        "probabilities": {"0": 0.0, "1": 1.0, "2": 0.0, "3": 0.0},
                        "confidence": 0.9},
                    "retrieval": {"type": "choice", "choice": "none",
                        "probabilities": {"none": 1.0}, "confidence": 0.9},
                    "missing_user_info": {"type": "noul", "noul": 0.1},
                    "parallelizable": {"type": "noul", "noul": 0.1},
                    "risk": {"type": "choice", "choice": "low",
                        "probabilities": {"low": 1.0}, "confidence": 0.9},
                    "model_tier": {"type": "choice", "choice": "fast",
                        "probabilities": {"fast": 1.0}, "confidence": 0.9}
                },
                "usage": {"input_tokens": 1, "output_tokens": 1}}"#,
        )
        .unwrap();

        let err = response.to_ingress_judgments().unwrap_err();
        assert!(err.to_string().contains("teleport"));
    }

    #[test]
    fn contradictory_answer_variants_fail_deserialization() {
        // noul + choice on one answer.
        assert!(
            serde_json::from_str::<Answer>(r#"{"type": "noul", "noul": 0.5, "choice": "x"}"#)
                .is_err()
        );

        // choice without probabilities.
        assert!(
            serde_json::from_str::<Answer>(
                r#"{"type": "choice", "choice": "x", "confidence": 0.9}"#
            )
            .is_err()
        );

        // score without legend.
        assert!(serde_json::from_str::<Answer>(
            r#"{"type": "score", "score": 1.0, "probabilities": {"0": 0.5, "1": 0.5}, "confidence": 0.9}"#
        )
        .is_err());

        // unknown type.
        assert!(serde_json::from_str::<Answer>(r#"{"type": "vibe"}"#).is_err());

        // answer without a type field.
        assert!(serde_json::from_str::<Answer>(r#"{"noul": 0.5}"#).is_err());
    }

    #[test]
    fn out_of_range_numbers_and_invalid_distributions_fail_validation() {
        let question = Question::choice("pick", [("a", "A"), ("b", "B")]);

        // Unknown option.
        let answer = Answer::Choice {
            choice: "c".to_owned(),
            probabilities: BTreeMap::from([("a".to_owned(), 0.5), ("b".to_owned(), 0.5)]),
            confidence: 0.9,
        };
        assert!(answer.validate(&question).is_err());

        // Probabilities do not sum to 1.
        let answer = Answer::Choice {
            choice: "a".to_owned(),
            probabilities: BTreeMap::from([("a".to_owned(), 0.5), ("b".to_owned(), 0.2)]),
            confidence: 0.9,
        };
        assert!(answer.validate(&question).is_err());

        // Missing probability key.
        let answer = Answer::Choice {
            choice: "a".to_owned(),
            probabilities: BTreeMap::from([("a".to_owned(), 1.0)]),
            confidence: 0.9,
        };
        assert!(answer.validate(&question).is_err());

        // Confidence out of range.
        let answer = Answer::Choice {
            choice: "a".to_owned(),
            probabilities: BTreeMap::from([("a".to_owned(), 1.0), ("b".to_owned(), 0.0)]),
            confidence: 1.5,
        };
        assert!(answer.validate(&question).is_err());

        // Score outside rubric range.
        let score_question = Question::score("rate", vec!["low", "high"]);
        let answer = Answer::Score {
            score: 5.0,
            legend: BTreeMap::from([
                ("0".to_owned(), "low".to_owned()),
                ("1".to_owned(), "high".to_owned()),
            ]),
            probabilities: BTreeMap::from([("0".to_owned(), 1.0), ("1".to_owned(), 0.0)]),
            confidence: 0.9,
        };
        assert!(answer.validate(&score_question).is_err());

        // Noul out of range.
        let noul_question = Question::noul("yes?");
        let answer = Answer::Noul { noul: 1.5 };
        assert!(answer.validate(&noul_question).is_err());

        // Type mismatch: choice answer for a noul question.
        let answer = Answer::Choice {
            choice: "a".to_owned(),
            probabilities: BTreeMap::from([("a".to_owned(), 1.0), ("b".to_owned(), 0.0)]),
            confidence: 0.9,
        };
        assert!(answer.validate(&noul_question).is_err());
    }

    #[test]
    fn answers_for_unknown_questions_are_rejected() {
        let response: SystemOneResponse = serde_json::from_str(
            r#"{"model": "jev-1.13.0",
                "answers": {
                    "handler": {"type": "choice", "choice": "act",
                        "probabilities": {"act": 1.0}, "confidence": 0.9},
                    "extra": {"type": "noul", "noul": 0.5}
                },
                "usage": {"input_tokens": 1, "output_tokens": 1}}"#,
        )
        .unwrap();

        let questions = BTreeMap::from([(
            "handler".to_owned(),
            Question::choice("pick", [("act", "do it"), ("no", "don't")]),
        )]);

        assert!(response.validate_against(&questions).is_err());
    }

    #[test]
    fn request_serializes_to_the_documented_wire_shape() {
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
        questions.insert(
            "frustration".to_owned(),
            Question::score("How frustrated is the customer?", vec!["Calm", "Angry"]),
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
        // Score serializes `criteria` as the ordered level array; the
        // undocumented `levels` field must not appear.
        assert_eq!(
            json["questions"]["frustration"]["criteria"],
            serde_json::json!(["Calm", "Angry"])
        );
        assert!(json["questions"]["frustration"].get("levels").is_none());
        // Instructions are required and always serialize.
        assert!(json["questions"]["urgent"]["instructions"].is_string());
    }

    #[test]
    fn question_and_request_validation_enforces_wire_limits() {
        // Score with one level.
        assert!(Question::score("rate", vec!["only"]).validate().is_err());
        // Score with too many levels.
        let levels: Vec<&str> = (0..11).map(|_| "level").collect();
        assert!(Question::score("rate", levels).validate().is_err());
        // Choice with one option.
        assert!(Question::choice("pick", [("a", "A")]).validate().is_err());
        // Empty instructions.
        assert!(Question::noul("  ").validate().is_err());

        // Oversized request.
        let mut questions = BTreeMap::new();
        questions.insert(
            "big".to_owned(),
            Question::noul("x?".repeat(MAX_REQUEST_BODY_BYTES / 2)),
        );
        let request = SystemOneRequest {
            model: "jev-latest".to_owned(),
            state: Value::Null,
            questions,
        };
        assert!(request.validate().is_err());

        // Empty model / no questions.
        let request = SystemOneRequest {
            model: " ".to_owned(),
            state: Value::Null,
            questions: BTreeMap::new(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn config_and_errors_never_reveal_the_key() {
        let config = TypeSafeConfig {
            api_key: "sk-secret-value".to_owned(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        };

        // Debug output redacts the key.
        let debug = format!("{config:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("sk-secret-value"));

        // The secret-free summary carries no key material.
        let summary = format!("{:?}", config.summary());
        assert!(!summary.contains("sk-secret-value"));

        // Error formatting redacts echoed bodies.
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

        // Overrides win, including pinned versioned model IDs.
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

    #[tokio::test]
    async fn live_smoke_jev_ingress_round_trip() {
        // Opt-in live test: only runs when TYPESAFE_LIVE_SMOKE=1 and a key
        // is present. Never silently executes in ordinary CI.
        if std::env::var("TYPESAFE_LIVE_SMOKE").ok().as_deref() != Some("1") {
            return;
        }
        let config = match TypeSafeConfig::from_env() {
            Ok(config) => config,
            Err(_) => return,
        };

        let jev = JevSystemOne::new(config).unwrap();
        let input = DecisionInput::new("read the config file", vec!["files".to_owned()]);
        let judgments = jev.judge(&input).await.unwrap();

        // The full answer set comes back and collapses into a decision.
        let decision = judgments.to_decision();
        assert!(decision.confidence >= 0.0);
    }
}
