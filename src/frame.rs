//! Coding-loop decision frames: Jev at meaningful observation boundaries,
//! not just prompt ingress (issue #22).
//!
//! A [`DecisionFrame`] is the compact, versioned evidence bundle a bounded
//! question is asked over: goal/turn revision, the current work unit, the
//! capabilities that actually exist, bounded candidate IDs, the latest
//! structured observation and what work remains. Questions select among
//! *runtime-authorized* candidates; they never invent tool arguments,
//! patches, permissions or proof of completion.
//!
//! Two rules keep this honest:
//! - a frame never offers a candidate the runtime could not dispatch
//!   (availability, policy and revision are checked before asking);
//! - an unusable answer (unknown choice, stale revision, transport
//!   failure) falls back to the safe path — reasoning — never to success.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{KnutError, Question};

/// Version of the frame and question-pack contract. Bump on breaking
/// changes so stored distributions stay attributable.
pub const FRAME_VERSION: u32 = 1;

/// One runtime-authorized option a frame may select.
///
/// The id is what Jev answers with; it must be resolvable back to a
/// concrete action by the runtime, or the answer is discarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub description: String,
}

impl Candidate {
    pub fn new(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
        }
    }
}

/// The escape hatch: Jev may always decline the candidate set.
///
/// Without this, a bad candidate list forces a plausible wrong choice.
pub const ESCALATE_ID: &str = "escalate";

/// What kind of decision is being asked, so results stay attributable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    /// Ingress: does this need known context or fresh reasoning?
    Ingress,
    /// Recovery: classify an observed failure.
    Recovery,
    /// Continue a known plan or ask for diagnosis/replan?
    Continuation,
    /// Pick among retrieval/tool candidates.
    CandidateSelection,
}

/// The compact evidence a bounded question is asked over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionFrame {
    /// Frame/question-pack version.
    pub version: u32,
    pub kind: FrameKind,
    /// Stable identity of the task this frame belongs to.
    pub task: u64,
    /// Task revision: an answer against an older revision is stale.
    pub revision: u64,
    /// Current work unit (node/step label), when there is one.
    pub unit: Option<String>,
    /// The goal, bounded.
    pub goal: String,
    /// Capabilities that exist and are authorized right now.
    pub capabilities: Vec<String>,
    /// Bounded candidates the runtime could dispatch.
    pub candidates: Vec<Candidate>,
    /// Latest structured observation (bounded summary, not the artifact).
    pub observation: Option<Value>,
    /// What still has to happen for the task to be complete.
    pub remaining: Vec<String>,
    /// Evidence references (check names and revisions), never raw blobs.
    pub evidence: Vec<String>,
}

impl DecisionFrame {
    pub fn new(kind: FrameKind, task: u64, revision: u64, goal: impl Into<String>) -> Self {
        Self {
            version: FRAME_VERSION,
            kind,
            task,
            revision,
            unit: None,
            goal: goal.into().chars().take(MAX_FIELD_CHARS).collect(),
            capabilities: Vec::new(),
            candidates: Vec::new(),
            observation: None,
            remaining: Vec::new(),
            evidence: Vec::new(),
        }
    }

    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    pub fn with_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.capabilities = capabilities.into_iter().map(Into::into).collect();
        self.capabilities.truncate(MAX_CANDIDATES);
        self
    }

    pub fn with_candidates(mut self, candidates: impl IntoIterator<Item = Candidate>) -> Self {
        self.candidates = candidates.into_iter().collect();
        // Bounded: a frame never grows with the repository.
        self.candidates.truncate(MAX_CANDIDATES);
        self
    }

    pub fn with_observation(mut self, observation: Value) -> Self {
        self.observation = Some(bounded(observation));
        self
    }

    pub fn with_remaining(
        mut self,
        remaining: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.remaining = remaining
            .into_iter()
            .map(|r| r.into().chars().take(MAX_FIELD_CHARS).collect())
            .collect();
        self.remaining.truncate(MAX_CANDIDATES);
        self
    }

    pub fn with_evidence(mut self, evidence: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.evidence = evidence
            .into_iter()
            .map(|e| e.into().chars().take(MAX_FIELD_CHARS).collect())
            .collect();
        self.evidence.truncate(MAX_CANDIDATES);
        self
    }

    /// Candidate ids, in the order presented.
    pub fn candidate_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect();
        ids.push(ESCALATE_ID.to_owned());
        ids
    }

    /// The state document sent to System One: compact, bounded, versioned.
    pub fn to_state(&self) -> Value {
        serde_json::json!({
            "frame_version": self.version,
            "kind": self.kind,
            "task": self.task,
            "revision": self.revision,
            "unit": self.unit,
            "goal": self.goal,
            "capabilities": self.capabilities,
            "candidates": self.candidates,
            "observation": self.observation,
            "remaining": self.remaining,
            "evidence": self.evidence,
        })
    }

    /// The coding question pack for this frame kind.
    ///
    /// Each pack is one independent fan-out: every question sees the same
    /// frame state and none consumes another's answer.
    pub fn questions(&self) -> BTreeMap<String, Question> {
        let mut questions = BTreeMap::new();

        match self.kind {
            FrameKind::Ingress => {
                questions.insert(
                    "context".to_owned(),
                    Question::choice(
                        "Does this request need known local context retrieved \
                         before reasoning, or is fresh reasoning the next step?",
                        [
                            (
                                "retrieve",
                                "Existing repository material must be found first",
                            ),
                            ("reason", "Understand, plan or generate directly"),
                            ("clarify", "Information only the user can supply is missing"),
                        ],
                    ),
                );
                questions.insert(
                    "needs_tools".to_owned(),
                    Question::noul(
                        "Does answering require executing a capability (tool) rather \
                         than only reasoning over provided evidence?",
                    ),
                );
            }
            FrameKind::Recovery => {
                questions.insert(
                    "failure_class".to_owned(),
                    Question::choice(
                        "Classify the observed failure.",
                        [
                            ("transient", "A timeout, rate limit or transport failure"),
                            (
                                "verification",
                                "The artifact was produced but failed a check",
                            ),
                            ("wrong_approach", "The approach itself was wrong"),
                            (
                                "blocked_by_policy",
                                "The action was refused by policy or approval",
                            ),
                            ("unknown", "The evidence does not determine a class"),
                        ],
                    ),
                );
                questions.insert(
                    "same_approach_retriable".to_owned(),
                    Question::noul(
                        "Can the same approach be retried as-is without risking a \
                         duplicated side effect?",
                    ),
                );
            }
            FrameKind::Continuation => {
                questions.insert(
                    "continue_plan".to_owned(),
                    Question::choice(
                        "Continue the known plan, or request diagnosis/replanning?",
                        [
                            ("continue", "The plan is still valid; run its next unit"),
                            ("replan", "New evidence invalidates the current plan"),
                            ("diagnose", "The failure is not understood yet"),
                            ("done", "The remaining work is provably complete"),
                        ],
                    ),
                );
            }
            FrameKind::CandidateSelection => {
                let options: Vec<(String, String)> = self
                    .candidates
                    .iter()
                    .map(|candidate| (candidate.id.clone(), candidate.description.clone()))
                    .chain(std::iter::once((
                        ESCALATE_ID.to_owned(),
                        "None of these is right; escalate to reasoning or ask the user".to_owned(),
                    )))
                    .collect();

                if options.len() >= 2 {
                    questions.insert(
                        "candidate".to_owned(),
                        Question::choice("Which authorized capability should act next?", options),
                    );
                }
            }
        }

        questions
    }
}

/// Bounded candidate count: a frame never offers an unbounded menu.
pub const MAX_CANDIDATES: usize = 12;
/// Bounded free-text field length.
pub const MAX_FIELD_CHARS: usize = 400;
/// Bounded serialized observation size.
pub const MAX_OBSERVATION_BYTES: usize = 4096;

/// Truncate a structural observation to a bounded, still-valid value.
fn bounded(value: Value) -> Value {
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    if serialized.len() <= MAX_OBSERVATION_BYTES {
        return value;
    }
    serde_json::json!({
        "truncated": true,
        "preview": serialized.chars().take(MAX_OBSERVATION_BYTES).collect::<String>(),
    })
}

/// A decided candidate, after validating the answer against the frame.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateChoice {
    pub id: String,
    /// True when the answer was `escalate`: route to reasoning, not to a
    /// guessed capability.
    pub escalate: bool,
    /// Raw answer distribution, preserved for traces and offline
    /// calibration (never collapsed into one number at the boundary).
    pub distribution: BTreeMap<String, f64>,
    pub confidence: f64,
}

impl CandidateChoice {
    /// The chosen candidate, or `None` when the answer escalates.
    pub fn chosen(&self) -> Option<&str> {
        if self.escalate {
            None
        } else {
            Some(self.id.as_str())
        }
    }
}

/// Validate a candidate answer against the frame that produced it.
///
/// Rejects an unknown candidate id outright: an answer that names
/// something the runtime did not offer cannot dispatch anything.
pub fn validate_candidate_answer(
    frame: &DecisionFrame,
    response: &crate::SystemOneResponse,
    answer_id: &str,
) -> Result<CandidateChoice, KnutError> {
    response.validate_against(&frame.questions())?;

    let answer = response
        .answers
        .get(answer_id)
        .ok_or_else(|| KnutError::SystemOne(format!("response has no answer for {answer_id:?}")))?;

    let (choice, distribution, confidence) = match answer {
        crate::Answer::Choice {
            choice,
            probabilities,
            confidence,
        } => (choice.clone(), probabilities.clone(), *confidence),
        other => {
            return Err(KnutError::SystemOne(format!(
                "expected a choice answer, got {other:?}"
            )));
        }
    };

    let offered = frame.candidate_ids();
    if !offered.contains(&choice) {
        return Err(KnutError::SystemOne(format!(
            "answer selected {choice:?}, which was not an offered candidate"
        )));
    }

    Ok(CandidateChoice {
        escalate: choice == ESCALATE_ID,
        id: choice,
        distribution,
        confidence,
    })
}

/// A System One backend that answers coding-frame questions.
///
/// The live implementation posts the frame's question pack to Jev; tests
/// and offline runs use [`StaticFrameRouter`]. Keeping this a trait means
/// the runtime never talks to a specific provider.
#[async_trait::async_trait]
pub trait FrameRouter: Send + Sync {
    /// Ask the frame's questions and return the validated answer set.
    async fn ask(&self, frame: &DecisionFrame) -> Result<crate::SystemOneResponse, KnutError>;
}

/// Deterministic frame answers for tests and offline runs.
#[derive(Debug, Clone)]
pub struct StaticFrameRouter {
    answers: std::collections::BTreeMap<String, crate::Answer>,
}

impl StaticFrameRouter {
    pub fn new(answers: std::collections::BTreeMap<String, crate::Answer>) -> Self {
        Self { answers }
    }

    /// A single choice answer.
    ///
    /// The raw distribution must cover every option the question offered,
    /// so the caller supplies the remaining probabilities. `confidence`
    /// is the probability of `value`.
    pub fn choice(id: impl Into<String>, value: impl Into<String>, confidence: f64) -> Self {
        let value = value.into();
        let mut probabilities = std::collections::BTreeMap::new();
        probabilities.insert(value.clone(), confidence);
        let mut answers = std::collections::BTreeMap::new();
        answers.insert(
            id.into(),
            crate::Answer::Choice {
                choice: value,
                probabilities,
                confidence,
            },
        );
        Self { answers }
    }

    /// A choice answer whose raw distribution covers every offered
    /// option: the chosen one plus the explicitly declared remainder.
    pub fn choice_over(
        id: impl Into<String>,
        value: impl Into<String>,
        confidence: f64,
        others: &[&str],
    ) -> Self {
        let value = value.into();
        let mut probabilities = std::collections::BTreeMap::new();
        probabilities.insert(value.clone(), confidence);
        let remaining = if others.is_empty() {
            0.0
        } else {
            (1.0 - confidence).max(0.0) / others.len() as f64
        };
        for other in others {
            probabilities.insert((*other).to_owned(), remaining);
        }
        let mut answers = std::collections::BTreeMap::new();
        answers.insert(
            id.into(),
            crate::Answer::Choice {
                choice: value,
                probabilities,
                confidence,
            },
        );
        Self { answers }
    }

    /// A boolean (noul) answer.
    pub fn noul(id: impl Into<String>, value: bool, confidence: f64) -> Self {
        let mut answers = std::collections::BTreeMap::new();
        answers.insert(
            id.into(),
            crate::Answer::Noul {
                noul: if value { confidence } else { 1.0 - confidence },
            },
        );
        Self { answers }
    }

    /// Merge another answer set into this one.
    pub fn and(mut self, other: StaticFrameRouter) -> Self {
        self.answers.extend(other.answers);
        self
    }
}

#[async_trait::async_trait]
impl FrameRouter for StaticFrameRouter {
    async fn ask(&self, frame: &DecisionFrame) -> Result<crate::SystemOneResponse, KnutError> {
        let questions = frame.questions();
        // Answer only what this frame actually asked.
        let answers = self
            .answers
            .iter()
            .filter(|(id, _)| questions.contains_key(*id))
            .map(|(id, answer)| (id.clone(), answer.clone()))
            .collect();
        Ok(crate::SystemOneResponse {
            resolved_model: "static-frame".to_owned(),
            answers,
            usage: crate::typesafe::Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        })
    }
}

/// Decide a candidate choice through a router, with an explicit safe
/// fallback: an unusable answer escalates to reasoning, never to a guess.
pub async fn decide_candidate<R: FrameRouter + ?Sized>(
    router: &R,
    frame: &DecisionFrame,
) -> Result<CandidateChoice, KnutError> {
    let response = router.ask(frame).await?;
    validate_candidate_answer(frame, &response, "candidate")
}

/// Read the `continue_plan` decision from a continuation frame.
pub async fn decide_continuation<R: FrameRouter + ?Sized>(
    router: &R,
    frame: &DecisionFrame,
) -> Result<String, KnutError> {
    let response = router.ask(frame).await?;
    response.validate_against(&frame.questions())?;
    let answer = response.answers.get("continue_plan").ok_or_else(|| {
        KnutError::SystemOne("response has no answer for \"continue_plan\"".to_owned())
    })?;
    answer.as_choice()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn frame(kind: FrameKind) -> DecisionFrame {
        DecisionFrame::new(kind, 7, 3, "fix the failing test")
            .with_unit("check")
            .with_capabilities(["files", "shell"])
            .with_candidates([
                Candidate::new("read", "Read the file under test"),
                Candidate::new("run_tests", "Run the test suite"),
            ])
    }

    #[test]
    fn frame_state_is_versioned_bounded_and_serializable() {
        let state = frame(FrameKind::Ingress).to_state();
        assert_eq!(state["frame_version"], json!(FRAME_VERSION));
        assert_eq!(state["revision"], json!(3));
        assert_eq!(state["kind"], json!("ingress"));
        assert_eq!(state["candidates"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn every_frame_offers_an_escalate_path() {
        // Without this, a bad candidate set forces a plausible wrong
        // choice.
        for kind in [
            FrameKind::Ingress,
            FrameKind::Recovery,
            FrameKind::Continuation,
            FrameKind::CandidateSelection,
        ] {
            let ids = frame(kind).candidate_ids();
            assert!(
                ids.contains(&ESCALATE_ID.to_owned()),
                "kind {kind:?} offered no escalate path"
            );
        }
    }

    #[test]
    fn candidate_selection_options_match_the_frame_plus_escalate() {
        let frame = frame(FrameKind::CandidateSelection);
        let questions = frame.questions();
        let question = questions.get("candidate").expect("candidate question");
        match question {
            Question::Choice { criteria, .. } => {
                let mut keys: Vec<&String> = criteria.keys().collect();
                keys.sort();
                assert_eq!(keys, vec!["escalate", "read", "run_tests"]);
            }
            other => panic!("expected a choice question, got {other:?}"),
        }
    }

    #[test]
    fn observations_are_bounded() {
        let huge = json!({ "content": "x".repeat(MAX_OBSERVATION_BYTES * 3) });
        let frame = DecisionFrame::new(FrameKind::Recovery, 1, 1, "g").with_observation(huge);
        let state = frame.to_state();
        assert_eq!(state["observation"]["truncated"], json!(true));
        assert!(
            serde_json::to_string(&state["observation"]).unwrap().len()
                <= MAX_OBSERVATION_BYTES + 200
        );
    }

    #[test]
    fn candidate_lists_are_bounded() {
        let many: Vec<Candidate> = (0..(MAX_CANDIDATES * 3))
            .map(|i| Candidate::new(format!("c{i}"), "x"))
            .collect();
        let frame =
            DecisionFrame::new(FrameKind::CandidateSelection, 1, 1, "g").with_candidates(many);
        assert_eq!(frame.candidates.len(), MAX_CANDIDATES);
    }

    #[test]
    fn ingress_and_recovery_packs_ask_different_questions() {
        let ingress = frame(FrameKind::Ingress).questions();
        assert!(ingress.contains_key("context"));
        assert!(ingress.contains_key("needs_tools"));

        let recovery = frame(FrameKind::Recovery).questions();
        assert!(recovery.contains_key("failure_class"));
        assert!(!recovery.contains_key("context"));

        // An ingress fan-out is still one round trip's worth of questions.
        assert_eq!(ingress.len(), 2);
        assert_eq!(recovery.len(), 2);
    }
}
