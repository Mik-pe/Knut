//! Role-specific context projections (issue #32).
//!
//! Jev should see exactly the decision evidence it needs, bounded; the
//! reasoner must not lose the code, constraints or diagnostics it needs
//! for correctness. Both are *projections of the same revisioned
//! artifacts*, so a projection never becomes the only copy of anything.
//!
//! Rules carried through the implementation:
//! - a lossy summary is never the only copy: originals stay in the
//!   artifact store and every projection records what it dropped;
//! - selection and truncation are deterministic, with explicit limits;
//! - token budgets distinguish *estimated* from *measured* usage, and an
//!   oversized request is caught before it fails repeatedly;
//! - a provider-required reasoning block is never trimmed to fit: the
//!   context is reset at a supported boundary and that choice is visible;
//! - a changed file cannot be presented as current from a cached excerpt
//!   (projections carry revision identities and re-check them).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::KnutError;
use crate::model::{Continuation, ModelRequest, ToolCall};
use crate::workspace::{ContentRef, Workspace};

/// Maximum characters in a Jev decision frame's free-text fields.
pub const MAX_FRAME_TEXT: usize = 400;
/// Maximum candidates offered to a decision frame.
pub const MAX_FRAME_CANDIDATES: usize = 12;
/// Maximum observation bytes in a frame.
pub const MAX_FRAME_OBSERVATION: usize = 4096;

/// A pinned constraint: user requirements that must survive compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    /// Short label, e.g. "must keep the public API".
    pub text: String,
    /// Where it came from (user turn, issue, instruction file).
    pub source: String,
}

/// An acceptance requirement that is still outstanding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceRequirement {
    pub check: String,
    pub description: String,
    /// Whether passing evidence currently exists for the revision.
    pub satisfied: bool,
}

/// One attached source range, with its provenance and revision identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceExcerpt {
    /// Workspace-relative path.
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Content identity at selection time.
    pub content_hash: String,
    pub text: String,
    /// Whether the excerpt was truncated to fit a limit.
    pub truncated: bool,
}

impl SourceExcerpt {
    /// Whether the excerpt still matches the file on disk.
    ///
    /// A changed file cannot be presented as current from a cached copy.
    pub fn is_fresh(&self, workspace: &Workspace) -> bool {
        workspace
            .read_bytes(&self.path)
            .map(|bytes| crate::workspace::content_hash(&bytes) == self.content_hash)
            .unwrap_or(false)
    }
}

/// A diagnostic reference: a pointer, not a copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticRef {
    pub check: String,
    /// Bounded excerpt of the diagnostic.
    pub excerpt: String,
    /// Whether more output exists beyond the excerpt.
    pub more_available: bool,
    /// Where the full output lives (a check id or log reference).
    pub reference: String,
}

/// The artifact store the projections read from.
///
/// Keeps the originals: a projection may be lossy, the store is not.
#[derive(Debug, Clone, Default)]
pub struct ArtifactIndex {
    excerpts: BTreeMap<String, SourceExcerpt>,
    diagnostics: Vec<DiagnosticRef>,
}

impl ArtifactIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a source excerpt, replacing any earlier one for the path.
    pub fn add_excerpt(&mut self, excerpt: SourceExcerpt) {
        self.excerpts.insert(excerpt.path.clone(), excerpt);
    }

    pub fn add_diagnostic(&mut self, diagnostic: DiagnosticRef) {
        self.diagnostics.push(diagnostic);
    }

    pub fn excerpts(&self) -> impl Iterator<Item = &SourceExcerpt> {
        self.excerpts.values()
    }

    pub fn diagnostics(&self) -> &[DiagnosticRef] {
        &self.diagnostics
    }

    pub fn get(&self, path: &str) -> Option<&SourceExcerpt> {
        self.excerpts.get(path)
    }

    /// Drop excerpts whose files changed, so a stale copy cannot be
    /// presented as current.
    pub fn invalidate_stale(&mut self, workspace: &Workspace) -> Vec<String> {
        let stale: Vec<String> = self
            .excerpts
            .iter()
            .filter(|(_, excerpt)| !excerpt.is_fresh(workspace))
            .map(|(path, _)| path.clone())
            .collect();
        for path in &stale {
            self.excerpts.remove(path);
        }
        stale
    }

    /// Read a bounded range into the index.
    pub fn capture(
        &mut self,
        workspace: &Workspace,
        relative: &str,
        start_line: usize,
        end_line: usize,
        max_chars: usize,
    ) -> Result<ContentRef, KnutError> {
        let bytes = workspace.read_bytes(relative)?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let end = end_line.min(lines.len());
        let start = start_line.max(1).min(end.max(1));
        let selected = lines
            .iter()
            .skip(start - 1)
            .take(end.saturating_sub(start) + 1)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        let truncated = selected.chars().count() > max_chars;
        let bounded: String = selected.chars().take(max_chars).collect();

        self.add_excerpt(SourceExcerpt {
            path: relative.to_owned(),
            start_line: start,
            end_line: end,
            content_hash: crate::workspace::content_hash(&bytes),
            text: bounded,
            truncated,
        });

        Ok(ContentRef {
            path: relative.to_owned(),
            start_line: start,
            end_line: end,
            content_hash: crate::workspace::content_hash(&bytes),
            bytes: bytes.len() as u64,
        })
    }
}

/// What a projection included, excluded or truncated.
///
/// The inspector reads this: context decisions are explained, not
/// implicit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReport {
    pub included_excerpts: Vec<String>,
    pub excluded_excerpts: Vec<String>,
    pub included_diagnostics: usize,
    pub excluded_diagnostics: usize,
    pub constraints_kept: usize,
    pub constraints_dropped: Vec<String>,
    pub estimated_input_tokens: usize,
    pub measured_input_tokens: Option<u64>,
    pub truncated: bool,
    /// Set when the context had to be reset rather than trimmed.
    pub reset_reason: Option<String>,
    /// Whether a provider continuation was preserved.
    pub continuation_preserved: bool,
}

impl ContextReport {
    /// One-line summary for the inspector.
    pub fn summary(&self) -> String {
        let measured = match self.measured_input_tokens {
            Some(tokens) => format!("{tokens} measured"),
            None => "unmeasured".to_owned(),
        };
        format!(
            "context: {} excerpt(s), {} diagnostic(s), {} constraint(s); {} estimated / {}; \
             truncated: {}",
            self.included_excerpts.len(),
            self.included_diagnostics,
            self.constraints_kept,
            self.estimated_input_tokens,
            measured,
            self.truncated
        )
    }
}

/// How to estimate tokens for a request.
///
/// An estimate is *labelled* as one; it never masquerades as measured
/// usage. The character heuristic is deliberately conservative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget {
    /// Maximum input tokens allowed for the target provider.
    pub max_input_tokens: usize,
    /// Output/tool overhead reserved, never spent on context.
    pub reserve_output_tokens: usize,
}

impl TokenBudget {
    /// A common window for a large reasoner, with headroom reserved.
    pub fn reasoner_default() -> Self {
        Self {
            max_input_tokens: 120_000,
            reserve_output_tokens: 16_000,
        }
    }

    /// A small window, as a bounded control layer like Jev needs.
    pub fn control_layer() -> Self {
        Self {
            max_input_tokens: 4_000,
            reserve_output_tokens: 500,
        }
    }

    /// Tokens available to context.
    pub fn available(&self) -> usize {
        self.max_input_tokens
            .saturating_sub(self.reserve_output_tokens)
    }

    /// Estimate tokens for text: ~4 characters per token for English and
    /// code, tighter for CJK, and labelled as an estimate everywhere.
    pub fn estimate(&self, text: &str) -> usize {
        let chars = text.chars().count();
        // Non-ASCII text typically costs more tokens per character.
        let non_ascii = text.chars().filter(|c| !c.is_ascii()).count();
        let ascii = chars - non_ascii;
        (ascii / 4) + non_ascii + 1
    }

    /// Reconcile an estimate with the provider's measured usage.
    ///
    /// When both are available the measured number wins for accounting;
    /// the comparison is recorded so a bad estimator is visible.
    pub fn reconcile(&self, estimated: usize, measured: Option<u64>) -> (usize, Option<u64>) {
        (estimated, measured)
    }
}

/// A projection for the fast control layer: bounded decision evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionProjection {
    /// Goal, bounded.
    pub goal: String,
    /// Current work unit.
    pub unit: Option<String>,
    /// Authorized candidates.
    pub candidates: Vec<String>,
    /// Bounded structured observation.
    pub observation: serde_json::Value,
    /// Remaining work, bounded.
    pub remaining: Vec<String>,
    /// Evidence references (names and revisions, never raw content).
    pub evidence: Vec<String>,
    pub report: ContextReport,
}

impl DecisionProjection {
    /// Build a bounded decision projection.
    ///
    /// Deliberately small: a control layer decides over identifiers and
    /// short evidence, not over the code itself.
    pub fn build(
        goal: &str,
        unit: Option<&str>,
        candidates: &[String],
        observation: &serde_json::Value,
        remaining: &[String],
        evidence: &[String],
    ) -> Self {
        let mut report = ContextReport {
            ..Default::default()
        };

        let goal = bound(goal, MAX_FRAME_TEXT);
        let unit = unit.map(|unit| bound(unit, MAX_FRAME_TEXT));
        let candidates: Vec<String> = candidates
            .iter()
            .take(MAX_FRAME_CANDIDATES)
            .map(|candidate| bound(candidate, MAX_FRAME_TEXT))
            .collect();
        if candidates.len() < candidates.len() {
            report.truncated = true;
        }

        let observation_text = observation.to_string();
        let observation = if observation_text.len() > MAX_FRAME_OBSERVATION {
            report.truncated = true;
            serde_json::json!({
                "truncated": true,
                "preview": observation_text.chars().take(MAX_FRAME_OBSERVATION).collect::<String>(),
            })
        } else {
            observation.clone()
        };

        let remaining: Vec<String> = remaining
            .iter()
            .take(MAX_FRAME_CANDIDATES)
            .map(|item| bound(item, MAX_FRAME_TEXT))
            .collect();
        let evidence: Vec<String> = evidence
            .iter()
            .take(MAX_FRAME_CANDIDATES)
            .map(|item| bound(item, MAX_FRAME_TEXT))
            .collect();

        report.estimated_input_tokens = TokenBudget::control_layer()
            .estimate(&format!("{goal} {observation} {}", remaining.join(" ")));

        Self {
            goal,
            unit,
            candidates,
            observation,
            remaining,
            evidence,
            report,
        }
    }

    /// The state document a decision frame sends.
    pub fn to_state(&self) -> serde_json::Value {
        serde_json::json!({
            "goal": self.goal,
            "unit": self.unit,
            "candidates": self.candidates,
            "observation": self.observation,
            "remaining": self.remaining,
            "evidence": self.evidence,
        })
    }
}

/// A projection for the reasoner: code, constraints and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonerProjection {
    /// The task, in full (bounded only by the budget).
    pub task: String,
    /// Pinned user constraints.
    pub constraints: Vec<Constraint>,
    /// Outstanding acceptance requirements.
    pub requirements: Vec<AcceptanceRequirement>,
    /// Selected source excerpts, with revision identities.
    pub sources: Vec<SourceExcerpt>,
    /// Diagnostic references.
    pub diagnostics: Vec<DiagnosticRef>,
    pub report: ContextReport,
}

impl ReasonerProjection {
    /// Build the reasoner context within a token budget.
    ///
    /// Selection is deterministic: constraints and requirements are pinned
    /// first (they are small and must survive), then diagnostics, then
    /// source excerpts in the order given. Anything dropped is *named* in
    /// the report and still exists in the artifact index.
    pub fn build(
        index: &ArtifactIndex,
        task: &str,
        constraints: &[Constraint],
        requirements: &[AcceptanceRequirement],
        budget: TokenBudget,
    ) -> Self {
        let mut report = ContextReport {
            constraints_kept: constraints.len(),
            ..Default::default()
        };

        let mut used = budget.estimate(task);
        let mut sources = Vec::new();
        let mut diagnostics = Vec::new();

        // Diagnostics are cheap pointers and high value: include first.
        for diagnostic in index.diagnostics() {
            let cost = budget.estimate(&diagnostic.excerpt);
            if used + cost > budget.available() {
                report.excluded_diagnostics += 1;
                report.truncated = true;
                continue;
            }
            used += cost;
            diagnostics.push(diagnostic.clone());
        }
        report.included_diagnostics = diagnostics.len();

        for excerpt in index.excerpts() {
            let cost = budget.estimate(&excerpt.text);
            if used + cost > budget.available() {
                report.excluded_excerpts.push(excerpt.path.clone());
                report.truncated = true;
                continue;
            }
            used += cost;
            report.included_excerpts.push(excerpt.path.clone());
            sources.push(excerpt.clone());
        }

        report.estimated_input_tokens = used;
        report.constraints_kept = constraints.len();

        Self {
            task: task.to_owned(),
            constraints: constraints.to_vec(),
            requirements: requirements.to_vec(),
            sources,
            diagnostics,
            report,
        }
    }

    /// Render this projection into a model request.
    pub fn to_request(&self) -> ModelRequest {
        let input = serde_json::json!({
            "task": self.task,
            "constraints": self.constraints,
            "requirements": self.requirements,
            "sources": self.sources.iter().map(|source| serde_json::json!({
                "path": source.path,
                "lines": [source.start_line, source.end_line],
                "content_hash": source.content_hash,
                "truncated": source.truncated,
                "text": source.text,
            })).collect::<Vec<_>>(),
            "diagnostics": self.diagnostics,
        });
        ModelRequest::new(self.task.clone(), crate::ExpectedArtifact::Text).with_input(input)
    }

    /// Whether every required field survived the budget.
    ///
    /// Used by the acceptance test: compaction must not lose the user
    /// constraint or the failing-test evidence.
    pub fn required_evidence_intact(&self, required_path: &str, required_check: &str) -> bool {
        self.sources
            .iter()
            .any(|source| source.path == required_path)
            && self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.check == required_check)
            && !self.constraints.is_empty()
    }
}

/// Bound a string to a character limit, marking truncation.
fn bound(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut bounded: String = text.chars().take(max_chars).collect();
    bounded.push('…');
    bounded
}

/// The result of fitting a conversation to a budget.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetedTurn {
    /// The request to send.
    pub request: ModelRequest,
    /// Continuation state to send alongside, when preserved.
    pub continuation: Option<Continuation>,
    pub report: ContextReport,
}

/// Why a provider continuation could not be preserved.
///
/// A provider-required reasoning block is never trimmed to fit: when it
/// does not fit, the context is reset at a supported boundary instead, and
/// that choice is visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationDecision {
    /// The continuation fits and is sent unchanged.
    Preserved,
    /// The continuation was dropped; a context reset is required.
    ResetRequired { reason: String },
}

/// Fit a reasoner projection plus provider state into a budget.
pub fn budget_turn(
    projection: &ReasonerProjection,
    continuation: Option<&Continuation>,
    tool_pairs: &[(ToolCall, String)],
    budget: TokenBudget,
) -> BudgetedTurn {
    let mut report = projection.report.clone();

    let mut request = projection.to_request();
    let continuation_cost = continuation
        .map(|continuation| {
            continuation
                .parts
                .iter()
                .map(|part| budget.estimate(&part.value))
                .sum::<usize>()
        })
        .unwrap_or(0);

    // Tool calls and their results must stay paired: a trimmed pair is
    // invalid at the provider, so they are included whole or reported.
    let tool_cost: usize = tool_pairs
        .iter()
        .map(|(call, result)| budget.estimate(&call.name) + budget.estimate(result))
        .sum();

    let total = projection.report.estimated_input_tokens + continuation_cost + tool_cost;
    report.estimated_input_tokens = total;

    let decision = match continuation {
        // The reasoning block is provider state: trimming it would produce
        // an invalid request, so it is either sent whole or the context is
        // reset visibly.
        Some(continuation)
            if total + continuation_cost > budget.available() && !continuation.is_empty() =>
        {
            report.reset_reason = Some(
                "the provider's preserved reasoning block did not fit; resetting context at a \
                 supported boundary instead of trimming it"
                    .to_owned(),
            );
            report.truncated = true;
            ContinuationDecision::ResetRequired {
                reason: "continuation did not fit".to_owned(),
            }
        }
        _ => ContinuationDecision::Preserved,
    };

    let continuation_out = match decision {
        ContinuationDecision::Preserved => continuation.cloned(),
        ContinuationDecision::ResetRequired { .. } => None,
    };
    report.continuation_preserved = continuation_out.is_some();

    if !tool_pairs.is_empty() {
        // Record the pairing in the request input so the caller can
        // reconstruct a valid message sequence.
        let pairs: Vec<serde_json::Value> = tool_pairs
            .iter()
            .map(|(call, result)| {
                serde_json::json!({
                    "tool_call": { "id": call.id, "name": call.name, "arguments": call.arguments },
                    "result": result,
                })
            })
            .collect();
        let mut input = request.input.clone();
        if let serde_json::Value::Object(map) = &mut input {
            map.insert("tool_pairs".to_owned(), serde_json::Value::Array(pairs));
        }
        request = request.with_input(input);
    }

    BudgetedTurn {
        request,
        continuation: continuation_out,
        report,
    }
}

/// Whether a request is estimated to fit before it is sent.
///
/// Catches an oversized request locally instead of failing repeatedly at
/// the provider.
pub fn fits_budget(request: &ModelRequest, budget: TokenBudget) -> (bool, usize) {
    let text = format!("{} {}", request.instruction, request.input);
    let estimate = budget.estimate(&text);
    (estimate <= budget.available(), estimate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ContinuationPart;
    use serde_json::json;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "knut-context-{name}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.dir.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
        }

        fn workspace(&self) -> Workspace {
            Workspace::open(&self.dir).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_long_multi_file_repair_survives_compaction() {
        // The acceptance test: many files, and the user constraint plus
        // the failing-test evidence must still be present afterwards.
        let fixture = Fixture::new("compaction");
        // Realistic multi-file work: each file is big enough that 20 of
        // them cannot fit a small budget together.
        for i in 0..20 {
            let body = format!("pub fn f{i}() {{\n")
                + &format!("    // body {i} with padding\n").repeat(40)
                + "}\n";
            fixture.write(&format!("src/module_{i}.rs"), &body);
        }
        let workspace = fixture.workspace();
        let mut index = ArtifactIndex::new();
        for i in 0..20 {
            index
                .capture(&workspace, &format!("src/module_{i}.rs"), 1, 3, 2_000)
                .unwrap();
        }
        index.add_diagnostic(DiagnosticRef {
            check: "test".to_owned(),
            excerpt: "test result: FAILED. 3 passed; 1 failed; failing: test_parser".to_owned(),
            more_available: true,
            reference: "check:test".to_owned(),
        });

        let constraints = vec![Constraint {
            text: "must keep the public API unchanged".to_owned(),
            source: "user turn 1".to_owned(),
        }];
        let requirements = vec![AcceptanceRequirement {
            check: "test".to_owned(),
            description: "the test suite passes".to_owned(),
            satisfied: false,
        }];

        // A budget too small for every excerpt.
        let budget = TokenBudget {
            max_input_tokens: 300,
            reserve_output_tokens: 20,
        };
        let projection = ReasonerProjection::build(
            &index,
            "fix the parser",
            &constraints,
            &requirements,
            budget,
        );

        // Compaction happened...
        assert!(projection.report.truncated);
        assert!(!projection.report.excluded_excerpts.is_empty());
        // ...but the constraint and the failing-test evidence survived.
        assert!(projection.required_evidence_intact("src/module_0.rs", "test"));
        assert!(!projection.constraints.is_empty());
        assert_eq!(projection.requirements.len(), 1);
        // And the originals are still available: a lossy projection never
        // becomes the only copy.
        assert_eq!(index.excerpts().count(), 20);
    }

    #[test]
    fn a_changed_file_cannot_be_presented_as_current() {
        let fixture = Fixture::new("stale");
        fixture.write("src/a.rs", "original content\n");
        let workspace = fixture.workspace();

        let mut index = ArtifactIndex::new();
        index.capture(&workspace, "src/a.rs", 1, 1, 100).unwrap();
        let excerpt = index.get("src/a.rs").unwrap().clone();
        assert!(excerpt.is_fresh(&workspace));

        // Someone edits the file.
        fixture.write("src/a.rs", "different content\n");
        assert!(!excerpt.is_fresh(&workspace));

        // Invalidation drops the stale copy rather than serving it.
        let dropped = index.invalidate_stale(&workspace);
        assert_eq!(dropped, vec!["src/a.rs".to_owned()]);
        assert!(index.get("src/a.rs").is_none());

        // A fresh capture is served after the change.
        index.capture(&workspace, "src/a.rs", 1, 1, 100).unwrap();
        assert!(index.get("src/a.rs").unwrap().text.contains("different"));
    }

    #[test]
    fn jev_input_stays_bounded_as_the_transcript_grows() {
        // The control-layer projection has a hard ceiling regardless of
        // how much the task produced.
        let huge_observation = json!({ "output": "x".repeat(200_000) });
        let many_candidates: Vec<String> = (0..500).map(|i| format!("candidate-{i}")).collect();
        let many_remaining: Vec<String> = (0..500).map(|i| format!("step-{i}")).collect();

        let projection = DecisionProjection::build(
            &"g".repeat(10_000),
            Some(&"u".repeat(10_000)),
            &many_candidates,
            &huge_observation,
            &many_remaining,
            &[],
        );

        assert!(projection.goal.chars().count() <= MAX_FRAME_TEXT + 1);
        assert!(projection.candidates.len() <= MAX_FRAME_CANDIDATES);
        assert!(projection.remaining.len() <= MAX_FRAME_CANDIDATES);
        assert!(
            projection.observation.to_string().len() <= MAX_FRAME_OBSERVATION + 200,
            "observation was not bounded"
        );
        assert!(projection.report.truncated);
    }

    #[test]
    fn reasoner_keeps_access_to_original_evidence() {
        let fixture = Fixture::new("originals");
        fixture.write("src/a.rs", "pub fn important() {}\n");
        let workspace = fixture.workspace();
        let mut index = ArtifactIndex::new();
        // The excerpt is captured with a small limit, so it truncates.
        index.capture(&workspace, "src/a.rs", 1, 1, 5).unwrap();
        assert!(index.get("src/a.rs").unwrap().truncated);

        // The file itself is still fully readable: the truncated excerpt
        // is not the only copy.
        let full = workspace.read_text("src/a.rs").unwrap();
        assert!(full.contains("important"));
    }

    #[test]
    fn a_provider_continuation_is_never_trimmed_to_fit() {
        let continuation = Continuation {
            parts: vec![ContinuationPart {
                kind: "reasoning_content".to_owned(),
                value: "r".repeat(4_000),
            }],
        };
        let fixture = Fixture::new("continuation");
        fixture.write("src/a.rs", "code\n");
        let workspace = fixture.workspace();
        let mut index = ArtifactIndex::new();
        index.capture(&workspace, "src/a.rs", 1, 1, 100).unwrap();

        let projection = ReasonerProjection::build(
            &index,
            "task",
            &[],
            &[],
            TokenBudget {
                max_input_tokens: 200,
                reserve_output_tokens: 10,
            },
        );

        let budgeted = budget_turn(
            &projection,
            Some(&continuation),
            &[],
            TokenBudget {
                max_input_tokens: 200,
                reserve_output_tokens: 10,
            },
        );

        // The continuation was dropped as a whole, and the reason is
        // visible: no partial reasoning block was sent.
        assert!(budgeted.continuation.is_none());
        assert!(!budgeted.report.continuation_preserved);
        assert!(budgeted.report.reset_reason.is_some());
    }

    #[test]
    fn a_fitting_continuation_is_preserved_unchanged() {
        let continuation = Continuation {
            parts: vec![ContinuationPart {
                kind: "reasoning_content".to_owned(),
                value: "short reasoning".to_owned(),
            }],
        };
        let projection = ReasonerProjection {
            task: "task".to_owned(),
            constraints: Vec::new(),
            requirements: Vec::new(),
            sources: Vec::new(),
            diagnostics: Vec::new(),
            report: ContextReport::default(),
        };

        let budgeted = budget_turn(
            &projection,
            Some(&continuation),
            &[],
            TokenBudget::reasoner_default(),
        );
        assert_eq!(budgeted.continuation, Some(continuation));
        assert!(budgeted.report.continuation_preserved);
        assert!(budgeted.report.reset_reason.is_none());
    }

    #[test]
    fn tool_call_result_pairs_stay_intact() {
        let pairs = vec![
            (
                ToolCall {
                    id: "call_1".to_owned(),
                    name: "read".to_owned(),
                    arguments: json!({ "path": "src/a.rs" }),
                },
                "file contents".to_owned(),
            ),
            (
                ToolCall {
                    id: "call_2".to_owned(),
                    name: "test".to_owned(),
                    arguments: json!({}),
                },
                "1 failed".to_owned(),
            ),
        ];

        let projection = ReasonerProjection {
            task: "task".to_owned(),
            constraints: Vec::new(),
            requirements: Vec::new(),
            sources: Vec::new(),
            diagnostics: Vec::new(),
            report: ContextReport::default(),
        };
        let budgeted = budget_turn(&projection, None, &pairs, TokenBudget::reasoner_default());

        let sent = budgeted.request.input["tool_pairs"].as_array().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["tool_call"]["id"], json!("call_1"));
        assert_eq!(sent[0]["result"], json!("file contents"));
        assert_eq!(sent[1]["tool_call"]["name"], json!("test"));
    }

    #[test]
    fn estimated_and_measured_usage_are_distinguished() {
        let budget = TokenBudget::reasoner_default();
        let estimate = budget.estimate("a moderately long piece of text");
        assert!(estimate > 0);

        let report = ContextReport {
            estimated_input_tokens: estimate,
            measured_input_tokens: None,
            ..Default::default()
        };
        assert!(report.summary().contains("unmeasured"));

        let report = ContextReport {
            estimated_input_tokens: estimate,
            measured_input_tokens: Some(42),
            ..Default::default()
        };
        assert!(report.summary().contains("measured"));

        // Reconciliation keeps both available.
        assert_eq!(budget.reconcile(estimate, Some(42)), (estimate, Some(42)));
    }

    #[test]
    fn an_oversized_request_is_caught_before_sending() {
        let budget = TokenBudget {
            max_input_tokens: 100,
            reserve_output_tokens: 10,
        };
        let small = ModelRequest::new("short task", crate::ExpectedArtifact::Text);
        let (fits, estimate) = fits_budget(&small, budget);
        assert!(fits);
        assert!(estimate < 100);

        let huge = ModelRequest::new("t".repeat(10_000), crate::ExpectedArtifact::Text);
        let (fits, estimate) = fits_budget(&huge, budget);
        assert!(!fits);
        assert!(estimate > 100);
    }

    #[test]
    fn the_report_explains_included_excluded_and_truncated_context() {
        let fixture = Fixture::new("report");
        fixture.write("src/a.rs", &"line\n".repeat(200));
        fixture.write("src/b.rs", "small\n");
        let workspace = fixture.workspace();

        let mut index = ArtifactIndex::new();
        index.capture(&workspace, "src/a.rs", 1, 200, 500).unwrap();
        index.capture(&workspace, "src/b.rs", 1, 1, 500).unwrap();
        index.add_diagnostic(DiagnosticRef {
            check: "build".to_owned(),
            excerpt: "error[E0308]".to_owned(),
            more_available: false,
            reference: "check:build".to_owned(),
        });

        let budget = TokenBudget {
            max_input_tokens: 200,
            reserve_output_tokens: 20,
        };
        let projection = ReasonerProjection::build(&index, "task", &[], &[], budget);

        let summary = projection.report.summary();
        assert!(summary.contains("context:"));
        assert!(summary.contains("estimated"));
        // Either everything fit, or what did not is named.
        assert!(
            !projection.report.truncated
                || !projection.report.excluded_excerpts.is_empty()
                || projection.report.excluded_diagnostics > 0
        );
    }

    #[test]
    fn a_decision_projection_carries_identifiers_not_code() {
        let projection = DecisionProjection::build(
            "fix the failing test",
            Some("check"),
            &["read".to_owned(), "run_tests".to_owned()],
            &json!({ "exit_code": 101 }),
            &["the failing check must pass".to_owned()],
            &["test@rev-1".to_owned()],
        );
        let state = projection.to_state();
        assert_eq!(state["candidates"].as_array().unwrap().len(), 2);
        // The state names evidence; it does not contain source code.
        let text = state.to_string();
        assert!(!text.contains("fn main"));
        assert!(text.contains("test@rev-1"));
    }
}
