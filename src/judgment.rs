use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Decision, DecisionInput, KnutError, RetrievalSource, Route, SystemOne};

/// One bounded answer: a choice plus how sure the judge was.
///
/// Raw per-judgment confidence is preserved for tracing and evals even
/// when the routing only consumes a subset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judgment<T> {
    pub choice: T,
    pub confidence: f32,
}

/// Which control path should handle the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Handler {
    Clarify,
    Retrieve,
    Act,
    Generate,
}

/// How much work the prompt looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Trivial,
    Routine,
    MultiStep,
    Deep,
}

/// Where retrieval should happen, including "nowhere".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalJudgment {
    None,
    Files,
    Memory,
    Web,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YesNo {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierJudgment {
    Fast,
    Standard,
    Reasoner,
}

/// The complete ingress answer set from one System One round trip.
///
/// Every field is always populated (observability); the decision derived
/// from it consumes only the fields relevant to the chosen handler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngressJudgments {
    pub handler: Judgment<Handler>,
    pub complexity: Judgment<Complexity>,
    pub retrieval: Judgment<RetrievalJudgment>,
    pub missing_user_info: Judgment<YesNo>,
    pub parallelizable: Judgment<YesNo>,
    pub risk: Judgment<crate::Risk>,
    pub model_tier: Judgment<TierJudgment>,
}

/// Named contradictions and their confidence dampening.
///
/// A contradiction does not invert the answer; it weakens it so the
/// runtime's confidence floor can escalate conservatively.
const CONTRADICTION_DAMPEN: f32 = 0.5;

impl IngressJudgments {
    /// Collapse the full answer set into a single routing `Decision`.
    ///
    /// Only fields relevant to the handler shape the outcome; everything
    /// else stays available on the judgments themselves. Confidence is the
    /// minimum across consumed judgments, halved once per named
    /// contradiction.
    pub fn to_decision(&self) -> Decision {
        let mut consumed: Vec<f32> = vec![self.handler.confidence];
        let mut contradictions = 0usize;

        // Contradiction: user-only information is missing but the judge
        // did not pick the clarify path.
        if self.missing_user_info.choice == YesNo::Yes && self.handler.choice != Handler::Clarify {
            contradictions += 1;
        }

        let route = match self.handler.choice {
            Handler::Clarify => Route::Clarify,
            Handler::Retrieve => Route::Retrieve,
            Handler::Act => Route::Act,
            Handler::Generate => Route::Generate,
        };

        // Retrieval source is only consumed when retrieving. A retrieve
        // handler with "no retrieval" is a contradiction.
        let retrieval = match self.handler.choice {
            Handler::Retrieve => {
                consumed.push(self.retrieval.confidence);
                let source = match self.retrieval.choice {
                    RetrievalJudgment::None => {
                        contradictions += 1;
                        RetrievalSource::Mixed
                    }
                    RetrievalJudgment::Files => RetrievalSource::Files,
                    RetrievalJudgment::Memory => RetrievalSource::Memory,
                    RetrievalJudgment::Web => RetrievalSource::Web,
                    RetrievalJudgment::Mixed => RetrievalSource::Mixed,
                };
                Some(source)
            }
            _ => None,
        };

        // Model tier is only consumed when generating. Deep work that
        // stays on the fast tier is a contradiction.
        let model_tier = match self.handler.choice {
            Handler::Generate => {
                consumed.push(self.model_tier.confidence);
                match self.model_tier.choice {
                    TierJudgment::Fast => {
                        if self.complexity.choice == Complexity::Deep {
                            contradictions += 1;
                        }
                        crate::ModelTier::Fast
                    }
                    TierJudgment::Standard => crate::ModelTier::Standard,
                    TierJudgment::Reasoner => crate::ModelTier::Reasoner,
                }
            }
            _ => crate::ModelTier::Fast,
        };

        // Act relies on capability discovery happening downstream; that is
        // a normal path, not a contradiction, so nothing is consumed there.

        let mut confidence = consumed.iter().copied().fold(f32::INFINITY, f32::min);
        for _ in 0..contradictions {
            confidence *= CONTRADICTION_DAMPEN;
        }

        Decision {
            route,
            confidence,
            retrieval,
            capability: None,
            model_tier,
            risk: self.risk.choice,
            parallelizable: self.parallelizable.choice == YesNo::Yes,
        }
    }
}

/// A System One backend that answers all ingress questions in one call.
#[async_trait]
pub trait JudgmentRouter: Send + Sync {
    async fn judge(&self, input: &DecisionInput) -> Result<IngressJudgments, KnutError>;
}

/// Adapter: run a batched judgment pass and collapse it into a `Decision`.
pub struct JudgmentSystemOne<J> {
    router: J,
}

impl<J> JudgmentSystemOne<J> {
    pub fn new(router: J) -> Self {
        Self { router }
    }
}

#[async_trait]
impl<J: JudgmentRouter> SystemOne for JudgmentSystemOne<J> {
    async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError> {
        Ok(self.router.judge(input).await?.to_decision())
    }
}

/// Deterministic judgment source for tests and local experiments.
#[derive(Debug, Clone)]
pub struct StaticJudgments {
    judgments: IngressJudgments,
}

impl StaticJudgments {
    pub fn new(judgments: IngressJudgments) -> Self {
        Self { judgments }
    }

    /// A confident, boring default: generate with the fast tier.
    pub fn confident_default() -> Self {
        Self::new(IngressJudgments {
            handler: Judgment {
                choice: Handler::Generate,
                confidence: 0.95,
            },
            complexity: Judgment {
                choice: Complexity::Routine,
                confidence: 0.9,
            },
            retrieval: Judgment {
                choice: RetrievalJudgment::None,
                confidence: 0.9,
            },
            missing_user_info: Judgment {
                choice: YesNo::No,
                confidence: 0.9,
            },
            parallelizable: Judgment {
                choice: YesNo::No,
                confidence: 0.9,
            },
            risk: Judgment {
                choice: crate::Risk::Low,
                confidence: 0.9,
            },
            model_tier: Judgment {
                choice: TierJudgment::Fast,
                confidence: 0.9,
            },
        })
    }
}

#[async_trait]
impl JudgmentRouter for StaticJudgments {
    async fn judge(&self, _input: &DecisionInput) -> Result<IngressJudgments, KnutError> {
        Ok(self.judgments.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judgments() -> IngressJudgments {
        IngressJudgments {
            handler: Judgment {
                choice: Handler::Retrieve,
                confidence: 0.9,
            },
            complexity: Judgment {
                choice: Complexity::Routine,
                confidence: 0.9,
            },
            retrieval: Judgment {
                choice: RetrievalJudgment::Files,
                confidence: 0.8,
            },
            missing_user_info: Judgment {
                choice: YesNo::No,
                confidence: 0.9,
            },
            parallelizable: Judgment {
                choice: YesNo::Yes,
                confidence: 0.7,
            },
            risk: Judgment {
                choice: crate::Risk::Low,
                confidence: 0.9,
            },
            model_tier: Judgment {
                choice: TierJudgment::Fast,
                confidence: 0.85,
            },
        }
    }

    #[test]
    fn retrieve_consumes_only_relevant_fields() {
        let decision = judgments().to_decision();

        // Confidence comes from handler (0.9) and retrieval (0.8), not
        // from the irrelevant model_tier judgment (0.85).
        assert_eq!(decision.route, Route::Retrieve);
        assert_eq!(decision.retrieval, Some(RetrievalSource::Files));
        assert_eq!(decision.confidence, 0.8);
        assert_eq!(decision.model_tier, crate::ModelTier::Fast);
    }

    #[test]
    fn generate_consumes_tier_not_retrieval() {
        let mut j = judgments();
        j.handler.choice = Handler::Generate;
        j.model_tier.choice = TierJudgment::Reasoner;

        let decision = j.to_decision();

        assert_eq!(decision.route, Route::Generate);
        assert_eq!(decision.retrieval, None);
        assert_eq!(decision.model_tier, crate::ModelTier::Reasoner);
        // min(handler 0.9, tier 0.85).
        assert!((decision.confidence - 0.85).abs() < 1e-6);
    }

    #[test]
    fn retrieve_with_no_source_dampens_confidence() {
        let mut j = judgments();
        j.retrieval.choice = RetrievalJudgment::None;

        let decision = j.to_decision();

        assert_eq!(decision.route, Route::Retrieve);
        assert_eq!(decision.retrieval, Some(RetrievalSource::Mixed));
        assert!((decision.confidence - 0.4).abs() < 1e-6);
    }

    #[test]
    fn missing_user_info_contradicts_non_clarify_handler() {
        let mut j = judgments();
        j.handler.choice = Handler::Act;
        j.missing_user_info.choice = YesNo::Yes;

        let decision = j.to_decision();

        assert_eq!(decision.route, Route::Act);
        // 0.9 (handler) halved once.
        assert!((decision.confidence - 0.45).abs() < 1e-6);
    }

    #[test]
    fn deep_work_on_fast_tier_dampens() {
        let mut j = judgments();
        j.handler.choice = Handler::Generate;
        j.complexity.choice = Complexity::Deep;
        j.model_tier.choice = TierJudgment::Fast;

        let decision = j.to_decision();

        assert_eq!(decision.model_tier, crate::ModelTier::Fast);
        // min(0.9, 0.85) halved once.
        assert!((decision.confidence - 0.425).abs() < 1e-6);
    }

    #[test]
    fn irrelevant_weak_judgments_do_not_weaken_the_route() {
        let mut j = judgments();
        j.model_tier.confidence = 0.05; // irrelevant for Retrieve
        j.parallelizable.confidence = 0.05; // never consumed

        let decision = j.to_decision();

        assert_eq!(decision.confidence, 0.8);
        // parallelizable comes from the choice, not its confidence.
        assert!(decision.parallelizable);

        // But the full answer set is preserved for observability.
        assert_eq!(j.parallelizable.choice, YesNo::Yes);
    }

    #[tokio::test]
    async fn adapter_produces_one_decision_from_one_round_trip() {
        let system_one = JudgmentSystemOne::new(StaticJudgments::confident_default());
        let decision = system_one
            .decide(&DecisionInput::new("hi", vec![]))
            .await
            .unwrap();

        assert_eq!(decision.route, Route::Generate);
        assert_eq!(decision.model_tier, crate::ModelTier::Fast);
        assert_eq!(decision.confidence, 0.9);
    }

    #[tokio::test]
    async fn weak_judgments_escalate_through_the_runtime() {
        let mut j = judgments();
        j.retrieval.choice = RetrievalJudgment::None;
        let knut = crate::Knut::new(JudgmentSystemOne::new(StaticJudgments::new(j)))
            .with_confidence_floor(0.75);

        let action = knut.next("find stuff", vec![]).await.unwrap();

        // Dampened to 0.4, below the floor: conservative escalation.
        assert_eq!(action, crate::Action::Generate(crate::ModelTier::Reasoner));
    }
}
