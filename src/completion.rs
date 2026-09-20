//! Verification-evidence contract (issue #16).
//!
//! `Done` is a deterministic conclusion, not a judgment: it becomes
//! legal only when every required check has passing evidence bound to
//! the exact artifact revision that would be shipped. Fake verifiers
//! exercise this contract now; real compiler/test runners implement the
//! same trait in M1.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Identity of the exact thing a check ran against.
///
/// Evidence is only valid for the revision it names: a test run against
/// yesterday's tree proves nothing about today's patch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ArtifactRevision {
    /// What was checked: a file path, a patch id, a built binary, ...
    pub artifact: String,
    /// Content identity of that artifact at check time (hash, version).
    pub revision: String,
}

impl ArtifactRevision {
    pub fn new(artifact: impl Into<String>, revision: impl Into<String>) -> Self {
        Self {
            artifact: artifact.into(),
            revision: revision.into(),
        }
    }
}

/// A single piece of verification evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Which check produced this evidence (e.g. "cargo-test").
    pub check: String,
    /// Exactly which artifact revision was checked.
    pub subject: ArtifactRevision,
    /// When the evidence was produced (ISO-8601 or monotonic label).
    pub produced_at: String,
    pub passed: bool,
    /// Structured detail (log excerpt, exit code, counts). Kept out of
    /// display identifiers; may contain user content.
    pub detail: serde_json::Value,
}

/// One requirement: a check that must have passing evidence for the
/// current artifact revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    pub check: String,
    /// "What does passing mean?" — shown to users and verifiers.
    pub description: String,
    /// Whether the requirement blocks completion or is advisory.
    pub blocking: bool,
}

/// The deterministic completion contract for a plan or session.
///
/// `satisfied` is the function the runtime consults before `Done` is
/// ever offered as an edge; a model's confidence plays no part in it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionRequirements {
    requirements: Vec<Requirement>,
}

impl CompletionRequirements {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn require(
        mut self,
        check: impl Into<String>,
        description: impl Into<String>,
        blocking: bool,
    ) -> Self {
        self.requirements.push(Requirement {
            check: check.into(),
            description: description.into(),
            blocking,
        });
        self
    }

    pub fn requirements(&self) -> &[Requirement] {
        &self.requirements
    }

    /// Are all blocking requirements satisfied by evidence bound to this
    /// exact artifact revision?
    ///
    /// Empty requirements are satisfied vacuously: an explicitly empty
    /// contract is legal (exploration tasks), an *unmet* one is not.
    pub fn satisfied(&self, evidence: &[Evidence], subject: &ArtifactRevision) -> bool {
        self.requirements.iter().filter(|r| r.blocking).all(|req| {
            evidence
                .iter()
                .any(|e| e.check == req.check && e.subject == *subject && e.passed)
        })
    }

    /// Missing blocking evidence for this revision, most useful first —
    /// the "what is still outstanding" list for escalation prompts.
    pub fn missing(&self, evidence: &[Evidence], subject: &ArtifactRevision) -> Vec<String> {
        self.requirements
            .iter()
            .filter(|r| r.blocking)
            .filter(|req| {
                !evidence
                    .iter()
                    .any(|e| e.check == req.check && e.subject == *subject && e.passed)
            })
            .map(|req| format!("{}: {}", req.check, req.description))
            .collect()
    }

    /// Advisory (non-blocking) requirement outcomes for the inspector.
    pub fn advisory(
        &self,
        evidence: &[Evidence],
        subject: &ArtifactRevision,
    ) -> BTreeMap<String, bool> {
        self.requirements
            .iter()
            .filter(|r| !r.blocking)
            .map(|req| {
                let passed = evidence
                    .iter()
                    .any(|e| e.check == req.check && e.subject == *subject && e.passed);
                (req.check.clone(), passed)
            })
            .collect()
    }
}

/// A verifier of artifacts: the trait real tooling implements.
///
/// Deterministic fake implementations let the whole completion flow be
/// exercised now; compilers and test runners arrive in M1 without
/// changing this contract.
#[async_trait::async_trait]
pub trait ArtifactVerifier: Send + Sync {
    fn checks_for(&self, subject: &ArtifactRevision) -> Vec<String>;

    async fn verify(&self, check: &str, subject: &ArtifactRevision) -> Evidence;
}

/// Collect evidence for every requirement against one revision.
pub async fn gather_evidence(
    verifier: &dyn ArtifactVerifier,
    requirements: &CompletionRequirements,
    subject: &ArtifactRevision,
) -> Vec<Evidence> {
    let mut evidence = Vec::new();
    for req in requirements.requirements() {
        if verifier.checks_for(subject).iter().any(|c| c == &req.check) {
            evidence.push(verifier.verify(&req.check, subject).await);
        }
    }
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn evidence(check: &str, revision: &str, passed: bool) -> Evidence {
        Evidence {
            check: check.to_owned(),
            subject: ArtifactRevision::new("src/lib.rs", revision),
            produced_at: "2026-09-20T12:00:00Z".to_owned(),
            passed,
            detail: json!({ "exit_code": if passed { 0 } else { 1 } }),
        }
    }

    fn requirements() -> CompletionRequirements {
        CompletionRequirements::none()
            .require("build", "the artifact compiles", true)
            .require("test", "the test suite passes", true)
            .require("lint-style", "formatting is tidy", false)
    }

    #[test]
    fn stale_revision_evidence_proves_nothing() {
        let current = ArtifactRevision::new("src/lib.rs", "rev-2");
        let reqs = requirements();

        // All checks passed, but against rev-1.
        let stale = vec![
            evidence("build", "rev-1", true),
            evidence("test", "rev-1", true),
        ];

        assert!(!reqs.satisfied(&stale, &current));
        assert_eq!(
            reqs.missing(&stale, &current),
            vec![
                "build: the artifact compiles".to_owned(),
                "test: the test suite passes".to_owned(),
            ]
        );
    }

    #[test]
    fn failing_or_missing_evidence_blocks_completion() {
        let current = ArtifactRevision::new("src/lib.rs", "rev-2");

        let failing_test = vec![
            evidence("build", "rev-2", true),
            evidence("test", "rev-2", false),
        ];
        assert!(!reqs_satisfied(&failing_test, &current));

        let missing_build = vec![evidence("test", "rev-2", true)];
        assert!(!reqs_satisfied(&missing_build, &current));

        let all_passing = vec![
            evidence("build", "rev-2", true),
            evidence("test", "rev-2", true),
            evidence("lint-style", "rev-2", false), // advisory: ignored
        ];
        assert!(reqs_satisfied(&all_passing, &current));
    }

    fn reqs_satisfied(evidence: &[Evidence], subject: &ArtifactRevision) -> bool {
        requirements().satisfied(evidence, subject)
    }

    #[test]
    fn advisory_requirements_do_not_block() {
        let current = ArtifactRevision::new("src/lib.rs", "rev-2");
        let reqs = requirements();

        let evidence = vec![
            evidence("build", "rev-2", true),
            evidence("test", "rev-2", true),
        ];

        assert!(reqs.satisfied(&evidence, &current));

        let advisory = reqs.advisory(&evidence, &current);
        assert_eq!(advisory.get("lint-style"), Some(&false));
    }

    #[test]
    fn empty_requirements_are_vacuously_satisfied() {
        let subject = ArtifactRevision::new("x", "1");
        assert!(CompletionRequirements::none().satisfied(&[], &subject));
    }

    /// A fake verifier exercises the full flow without real tooling.
    #[tokio::test]
    async fn fake_verifier_gathers_evidence_for_all_requirements() {
        struct FakeVerifier {
            revision: String,
            fail_build: bool,
        }

        #[async_trait::async_trait]
        impl ArtifactVerifier for FakeVerifier {
            fn checks_for(&self, _subject: &ArtifactRevision) -> Vec<String> {
                vec![
                    "build".to_owned(),
                    "test".to_owned(),
                    "lint-style".to_owned(),
                ]
            }

            async fn verify(&self, check: &str, subject: &ArtifactRevision) -> Evidence {
                assert_eq!(subject.revision, self.revision);
                Evidence {
                    check: check.to_owned(),
                    subject: subject.clone(),
                    produced_at: "now".to_owned(),
                    passed: !(check == "build" && self.fail_build),
                    detail: json!({}),
                }
            }
        }

        let subject = ArtifactRevision::new("src/lib.rs", "rev-9");
        let reqs = requirements();
        let verifier = FakeVerifier {
            revision: "rev-9".to_owned(),
            fail_build: true,
        };

        let evidence = gather_evidence(&verifier, &reqs, &subject).await;
        assert!(!reqs.satisfied(&evidence, &subject));

        let verifier = FakeVerifier {
            revision: "rev-9".to_owned(),
            fail_build: false,
        };
        let evidence = gather_evidence(&verifier, &reqs, &subject).await;
        assert!(reqs.satisfied(&evidence, &subject));
    }

    #[test]
    fn evidence_and_revisions_are_serializable() {
        let subject = ArtifactRevision::new("src/lib.rs", "rev-2");
        let json = serde_json::to_string(&evidence("build", "rev-2", true)).unwrap();
        assert!(json.contains("rev-2"));

        let round: Evidence = serde_json::from_str(&json).unwrap();
        assert_eq!(round.subject, subject);
        assert!(round.passed);
    }
}
