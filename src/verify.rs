//! Revision-bound verification evidence and substantive reasoner review
//! (issue #26).
//!
//! "Valid JSON" is not "correct code". This module separates three
//! things that are easy to blur together:
//!
//! 1. **artifact/schema validation** — the patch parsed and applied;
//! 2. **check evidence** — a build/typecheck/test/lint command actually
//!    ran against a specific source revision and reported what happened;
//! 3. **semantic review** — the configured reasoner judged whether the
//!    change satisfies the task, which is *additional* evidence, never a
//!    substitute for checks.
//!
//! Completion requires (2) fresh for the current revision, and a check
//! outcome is one of passed / failed / skipped / unavailable / stale /
//! inconclusive. "No tests were discovered" is **not** passing, and a
//! Jev judgment cannot override the evidence gate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::sandbox::{CommandOutcome, CommandRequest, CommandStatus, Supervisor};
use crate::workspace::{Workspace, content_hash};
use crate::{
    ArtifactRevision, CompletionRequirements, Evidence, ExpectedArtifact, KnutError, ModelRequest,
    ModelResponse, Verifier,
};

/// A check's structured outcome. Never a boolean: "we did not run it" and
/// "it ran and passed" are different facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Failed,
    /// The runner declined to run it (disabled, ignored, not applicable).
    Skipped,
    /// The tool is not installed or the check cannot run here.
    Unavailable,
    /// The check would answer about a different revision.
    Stale,
    /// It ran but the result cannot be interpreted (malformed output,
    /// no tests discovered, ambiguous exit).
    Inconclusive,
}

impl CheckOutcome {
    pub fn is_green(self) -> bool {
        matches!(self, CheckOutcome::Passed)
    }

    /// Whether this outcome may satisfy a blocking requirement.
    pub fn satisfies_requirement(self) -> bool {
        self.is_green()
    }
}

/// One configured check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    /// Stable name used in requirements (e.g. "test", "build").
    pub name: String,
    pub description: String,
    pub program: String,
    pub args: Vec<String>,
    /// Exit codes considered success (the runner's own contract).
    pub success_codes: Vec<i32>,
    pub timeout_secs: u64,
    /// Whether the check needs to write in the workspace (builds do).
    pub writable_paths: Vec<String>,
    /// Text that must appear in the output for the check to count as
    /// having actually run (e.g. a test-count line). Empty means the exit
    /// code alone decides.
    pub expect_output_contains: Vec<String>,
    /// Whether a check whose output contains none of its expected markers
    /// must be reported as inconclusive rather than passed. This is how
    /// "no tests discovered" stops being a green result.
    pub require_output_marker: bool,
}

impl CheckSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        program: impl Into<String>,
        args: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            program: program.into(),
            args,
            success_codes: vec![0],
            timeout_secs: 600,
            writable_paths: Vec::new(),
            expect_output_contains: Vec::new(),
            require_output_marker: false,
        }
    }

    pub fn with_writable(mut self, paths: Vec<String>) -> Self {
        self.writable_paths = paths;
        self
    }

    pub fn with_success_codes(mut self, codes: Vec<i32>) -> Self {
        self.success_codes = codes;
        self
    }

    pub fn with_expected_output(mut self, markers: Vec<String>) -> Self {
        self.expect_output_contains = markers;
        self.require_output_marker = true;
        self
    }
}

/// A profile: the checks a workspace's language/toolchain suggests.
///
/// Discovered, never guessed: the profile is chosen from evidence in the
/// workspace (a `Cargo.toml`, a `package.json` with a test script), and
/// running it still needs the same approval as any other command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckProfile {
    pub name: String,
    pub checks: Vec<CheckSpec>,
}

impl CheckProfile {
    /// Rust: build, test, clippy.
    pub fn rust() -> Self {
        Self {
            name: "rust".to_owned(),
            checks: vec![
                CheckSpec::new(
                    "build",
                    "the crate compiles",
                    "cargo",
                    vec!["build".to_owned(), "--quiet".to_owned()],
                )
                .with_writable(vec![".".to_owned(), "target".to_owned()]),
                CheckSpec::new(
                    "test",
                    "the test suite passes",
                    "cargo",
                    vec!["test".to_owned(), "--quiet".to_owned()],
                )
                .with_writable(vec![".".to_owned(), "target".to_owned()])
                // A run with no tests at all must not look like passing.
                .with_expected_output(vec!["test result:".to_owned()]),
                CheckSpec::new(
                    "lint",
                    "clippy reports no warnings",
                    "cargo",
                    vec![
                        "clippy".to_owned(),
                        "--all-targets".to_owned(),
                        "--".to_owned(),
                        "-D".to_owned(),
                        "warnings".to_owned(),
                    ],
                )
                .with_writable(vec![".".to_owned(), "target".to_owned()]),
            ],
        }
    }

    /// TypeScript/Node: typecheck plus the project's test script.
    pub fn typescript() -> Self {
        Self {
            name: "typescript".to_owned(),
            checks: vec![
                CheckSpec::new(
                    "typecheck",
                    "TypeScript compiles without type errors",
                    "npx",
                    vec!["tsc".to_owned(), "--noEmit".to_owned()],
                )
                .with_writable(vec![".".to_owned(), "node_modules".to_owned()]),
                CheckSpec::new(
                    "test",
                    "the test suite passes",
                    "npm",
                    vec!["test".to_owned(), "--silent".to_owned()],
                )
                .with_writable(vec![".".to_owned(), "node_modules".to_owned()]),
            ],
        }
    }

    pub fn for_workspace(workspace: &Workspace) -> Result<Self, KnutError> {
        let profiles = discover_profiles(workspace);
        if profiles.is_empty() {
            return Err(KnutError::Tool("No repository checks configured: expected Cargo.toml, package.json, or tsconfig.json".to_owned()));
        }
        let multiple = profiles.len() > 1;
        let mut checks = Vec::new();
        for profile in profiles {
            for mut check in profile.checks {
                if multiple {
                    check.name = format!("{}:{}", profile.name, check.name);
                }
                checks.push(check);
            }
        }
        Ok(Self {
            name: "workspace".to_owned(),
            checks,
        })
    }

    /// Whether this profile's primary toolchain is present.
    pub fn tool_available(&self) -> bool {
        self.checks
            .first()
            .is_some_and(|check| executable_exists(&check.program))
    }
}

/// Discover the profiles that fit a workspace, without guessing.
///
/// Returns every profile whose *marker file* is present; a workspace with
/// both `Cargo.toml` and `package.json` legitimately has both. Nothing is
/// executed here.
pub fn discover_profiles(workspace: &Workspace) -> Vec<CheckProfile> {
    let mut profiles = Vec::new();
    if workspace.root().join("Cargo.toml").is_file() {
        profiles.push(CheckProfile::rust());
    }
    if workspace.root().join("package.json").is_file()
        || workspace.root().join("tsconfig.json").is_file()
    {
        profiles.push(CheckProfile::typescript());
    }
    profiles
}

fn executable_exists(program: &str) -> bool {
    if program.contains('/') {
        return std::path::Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Parsed test-runner output, so "ran but discovered nothing" is visible.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCounts {
    pub passed: Option<u64>,
    pub failed: Option<u64>,
    pub ignored: Option<u64>,
}

impl TestCounts {
    /// Parse the common `test result: ok. 12 passed; 0 failed; 1 ignored`
    /// line (cargo) and Jest/Vitest-style totals.
    pub fn parse(output: &str) -> Self {
        // A run reports one summary line per test binary; they add up.
        // Counting starts at zero but stays "unknown" until a summary is
        // actually seen, so a silent stream is never mistaken for a pass.
        let mut counts = TestCounts {
            passed: Some(0),
            failed: Some(0),
            ignored: Some(0),
        };
        let mut saw_summary = false;

        for line in output.lines() {
            if let Some(rest) = line.split("test result:").nth(1) {
                saw_summary = true;
                // Segments look like "ok. 12 passed", "0 failed",
                // "1 ignored": the number is followed by its label.
                for part in rest.split(';') {
                    let words: Vec<&str> = part.split_whitespace().collect();
                    let Some(index) = words.iter().position(|w| w.parse::<u64>().is_ok()) else {
                        continue;
                    };
                    let Ok(number) = words[index].parse::<u64>() else {
                        continue;
                    };
                    match words.get(index + 1).map(|w| w.trim_end_matches('.')) {
                        Some("passed") => counts.passed = counts.passed.map(|v| v + number),
                        Some("failed") => counts.failed = counts.failed.map(|v| v + number),
                        Some("ignored") => counts.ignored = counts.ignored.map(|v| v + number),
                        _ => {}
                    }
                }
            }

            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("Tests:") {
                saw_summary = true;
                // Jest prints `Tests: 3 failed, 4 passed, 7 total`.
                for part in rest
                    .split([',', ':'])
                    .flat_map(|segment| segment.split("  "))
                {
                    let part = part.trim();
                    let mut words = part.split_whitespace();
                    let Some(number) = words.next().and_then(|n| n.parse::<u64>().ok()) else {
                        continue;
                    };
                    match words.next() {
                        Some("passed") => counts.passed = Some(number),
                        Some("failed") => counts.failed = Some(number),
                        Some("skipped") | Some("todo") => counts.ignored = Some(number),
                        _ => {}
                    }
                }
            }
        }

        if !saw_summary {
            // Nothing was reported: unknown, not zero.
            return TestCounts::default();
        }
        counts
    }

    /// Whether any test actually ran.
    pub fn ran_any(&self) -> bool {
        self.passed.unwrap_or(0) + self.failed.unwrap_or(0) > 0
    }
}

/// One executed check, with its revision binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckEvidence {
    pub name: String,
    pub description: String,
    pub outcome: CheckOutcome,
    /// The exact source revision this ran against.
    pub revision: ArtifactRevision,
    /// The command that ran, as an explicit argv.
    pub command: Vec<String>,
    /// Exit code when the process exited.
    pub exit_code: Option<i32>,
    /// Signal, when it was signalled.
    pub signal: Option<i32>,
    pub duration_ms: u64,
    /// Bounded, sanitized output reference.
    pub output: String,
    pub output_truncated: bool,
    pub test_counts: TestCounts,
    /// Why the outcome is what it is (especially for non-green ones).
    pub reason: String,
}

impl CheckEvidence {
    /// Project into the completion contract's evidence type.
    pub fn to_evidence(&self) -> Evidence {
        Evidence {
            check: self.name.clone(),
            subject: self.revision.clone(),
            produced_at: format!("monotonic:{}ms", self.duration_ms),
            passed: self.outcome.satisfies_requirement(),
            detail: serde_json::to_value(self).unwrap_or(Value::Null),
        }
    }
}

/// Detect checks that were deleted, disabled or weakened.
///
/// A repair that removes the failing test is not a repair: the caller
/// compares the acceptance set before and after and refuses to count a
/// weakened suite as success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSetChange {
    pub removed: Vec<String>,
    pub added: Vec<String>,
    /// Tests that are present but now ignored/disabled.
    pub newly_ignored: Vec<String>,
}

impl TestSetChange {
    /// Whether the change weakens the acceptance set.
    pub fn weakens(&self) -> bool {
        !self.removed.is_empty() || !self.newly_ignored.is_empty()
    }

    /// Compare two sets of test names.
    pub fn compare(before: &[String], after: &[String]) -> Self {
        let mut removed = Vec::new();
        let mut newly_ignored = Vec::new();
        for name in before {
            // A test marked ignored keeps its bare name and gains an
            // `ignored:` prefix, so match on the stripped form.
            let stripped = |candidate: &String| candidate.trim_start_matches("ignored:").to_owned();
            let present = after.iter().any(|candidate| stripped(candidate) == *name);
            let disabled = after
                .iter()
                .any(|candidate| candidate.starts_with("ignored:") && stripped(candidate) == *name);
            if disabled {
                newly_ignored.push(name.clone());
            } else if !present {
                removed.push(name.clone());
            }
        }
        let added = after
            .iter()
            .map(|name| name.trim_start_matches("ignored:").to_owned())
            .filter(|name| !before.contains(name))
            .collect();
        Self {
            removed,
            added,
            newly_ignored,
        }
    }
}

/// Runs configured checks and produces revision-bound evidence.
pub struct CheckRunner {
    workspace: Workspace,
    supervisor: Arc<Supervisor>,
    profile: CheckProfile,
}

impl CheckRunner {
    pub fn new(workspace: Workspace, supervisor: Arc<Supervisor>, profile: CheckProfile) -> Self {
        Self {
            workspace,
            supervisor,
            profile,
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn profile(&self) -> &CheckProfile {
        &self.profile
    }

    /// The requirements this profile implies for a revision.
    pub fn requirements(&self) -> CompletionRequirements {
        let mut requirements = CompletionRequirements::none();
        for check in &self.profile.checks {
            // Build and test gate completion; lint is advisory so a style
            // warning is visible without blocking a correct fix.
            let blocking = !matches!(check.name.rsplit(':').next(), Some("lint"));
            requirements =
                requirements.require(check.name.clone(), check.description.clone(), blocking);
        }
        requirements
    }

    /// Run one check against a revision.
    pub async fn run_check(
        &self,
        check: &CheckSpec,
        revision: &ArtifactRevision,
    ) -> Result<CheckEvidence, KnutError> {
        // Unavailable tool: reported, never a green result.
        if !executable_exists(&check.program) {
            return Ok(CheckEvidence {
                name: check.name.clone(),
                description: check.description.clone(),
                outcome: CheckOutcome::Unavailable,
                revision: revision.clone(),
                command: std::iter::once(check.program.clone())
                    .chain(check.args.iter().cloned())
                    .collect(),
                exit_code: None,
                signal: None,
                duration_ms: 0,
                output: format!("{} is not installed", check.program),
                output_truncated: false,
                test_counts: TestCounts::default(),
                reason: format!("{} is not installed", check.program),
            });
        }

        let mut request = CommandRequest::new(check.program.clone(), check.args.clone())
            .with_timeout(Duration::from_secs(check.timeout_secs))
            .with_writable(check.writable_paths.clone());
        // Repository scripts run with the repository's own tool settings
        // but a clean environment, and no network by default.
        request
            .env
            .insert("CARGO_TERM_COLOR".to_owned(), "never".to_owned());

        let outcome = self.supervisor.run(request).await?;
        Ok(self.evidence_for(check, revision, outcome))
    }

    fn evidence_for(
        &self,
        check: &CheckSpec,
        revision: &ArtifactRevision,
        outcome: CommandOutcome,
    ) -> CheckEvidence {
        let combined = format!("{}\n{}", outcome.stdout.text, outcome.stderr.text);
        let counts = TestCounts::parse(&combined);
        let command: Vec<String> = std::iter::once(outcome.program.clone())
            .chain(outcome.args.iter().cloned())
            .collect();

        let (outcome_kind, exit_code, signal, reason) = match &outcome.status {
            CommandStatus::Exited { code } => {
                if check.success_codes.contains(code) {
                    // An exit code of 0 is not enough when the check
                    // declares output markers: a suite that ran nothing
                    // must not look green.
                    if check.require_output_marker && !markers_present(check, &combined) {
                        (
                            CheckOutcome::Inconclusive,
                            Some(*code),
                            None,
                            "the command succeeded but produced none of its expected output markers"
                                .to_owned(),
                        )
                    } else if check.name.rsplit(':').next() == Some("test") && !counts.ran_any() {
                        (
                            CheckOutcome::Inconclusive,
                            Some(*code),
                            None,
                            "the test runner reported no tests at all".to_owned(),
                        )
                    } else if check.name.rsplit(':').next() == Some("test")
                        && counts.failed.unwrap_or(0) > 0
                    {
                        (
                            CheckOutcome::Failed,
                            Some(*code),
                            None,
                            format!("{} test(s) failed", counts.failed.unwrap_or(0)),
                        )
                    } else {
                        (CheckOutcome::Passed, Some(*code), None, "passed".to_owned())
                    }
                } else {
                    (
                        CheckOutcome::Failed,
                        Some(*code),
                        None,
                        format!("exited with code {code}"),
                    )
                }
            }
            CommandStatus::Signalled { signal } => (
                CheckOutcome::Failed,
                None,
                Some(*signal),
                format!("killed by signal {signal}"),
            ),
            CommandStatus::TimedOut => (
                CheckOutcome::Inconclusive,
                None,
                None,
                "the check exceeded its time limit".to_owned(),
            ),
            CommandStatus::Cancelled => (
                CheckOutcome::Inconclusive,
                None,
                None,
                "the check was cancelled".to_owned(),
            ),
            CommandStatus::SandboxRefused { reason } => (
                CheckOutcome::Unavailable,
                None,
                None,
                format!("sandbox refused the check: {reason}"),
            ),
            CommandStatus::UnknownEffect { reason } => (
                CheckOutcome::Inconclusive,
                None,
                None,
                format!("the check's outcome is unknown: {reason}"),
            ),
        };

        CheckEvidence {
            name: check.name.clone(),
            description: check.description.clone(),
            outcome: outcome_kind,
            revision: revision.clone(),
            command,
            exit_code,
            signal,
            duration_ms: outcome.duration_ms,
            output: combined.chars().take(20_000).collect(),
            output_truncated: outcome.stdout.truncated || outcome.stderr.truncated,
            test_counts: counts,
            reason,
        }
    }

    /// Run every check in the profile against one revision.
    pub async fn run_all(&self, revision: &ArtifactRevision) -> Vec<CheckEvidence> {
        let mut evidence = Vec::new();
        for check in &self.profile.checks {
            match self.run_check(check, revision).await {
                Ok(item) => evidence.push(item),
                Err(err) => evidence.push(CheckEvidence {
                    name: check.name.clone(),
                    description: check.description.clone(),
                    outcome: CheckOutcome::Unavailable,
                    revision: revision.clone(),
                    command: std::iter::once(check.program.clone())
                        .chain(check.args.iter().cloned())
                        .collect(),
                    exit_code: None,
                    signal: None,
                    duration_ms: 0,
                    output: err.to_string(),
                    output_truncated: false,
                    test_counts: TestCounts::default(),
                    reason: format!("the check could not be started: {err}"),
                }),
            }
        }
        evidence
    }

    /// The source revision currently on disk, so evidence is bound to what
    /// is actually there.
    pub fn current_revision(
        &self,
        label: impl Into<String>,
    ) -> Result<ArtifactRevision, KnutError> {
        // Hash the tracked project files, so the revision changes when
        // any of them does. Ignore files are honored the same way the
        // read/search tools honor them.
        let mut files: Vec<(String, String)> = Vec::new();
        let walker = ignore::WalkBuilder::new(self.workspace.root())
            .standard_filters(true)
            .require_git(false)
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(self.workspace.root()) else {
                continue;
            };
            let relative = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if self.workspace.is_denied(&relative) {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            // Skip oversized artifacts (build outputs) so the revision
            // tracks source, not binaries.
            if bytes.len() as u64 > 512 * 1024 {
                continue;
            }
            files.push((relative, content_hash(&bytes)));
        }
        if files.is_empty() {
            return Err(KnutError::Tool(
                "no files to derive a revision from".to_owned(),
            ));
        }
        // Paths are sorted, so the revision depends on content only.
        files.sort();
        let serialized = serde_json::to_vec(&files).unwrap_or_default();
        Ok(ArtifactRevision::new(
            label.into(),
            content_hash(&serialized),
        ))
    }
}

fn markers_present(check: &CheckSpec, output: &str) -> bool {
    check.expect_output_contains.is_empty()
        || check
            .expect_output_contains
            .iter()
            .any(|marker| output.contains(marker))
}

/// The verdict of a substantive semantic review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Accept,
    Concerns,
    Reject,
}

/// A reasoner review of a change: task constraints, unintended behavior,
/// scope and unexplained changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewOutcome {
    pub verdict: ReviewVerdict,
    /// Specific concerns, when any.
    pub concerns: Vec<String>,
    /// Whether the model's answer was interpretable at all. An
    /// uninterpretable review is *not* an acceptance.
    pub parsed: bool,
    pub identity: String,
}

impl ReviewOutcome {
    /// Whether this review may count toward completion.
    ///
    /// A review is additional evidence: it can never replace checks, and
    /// an unparsed one is never an acceptance.
    pub fn is_acceptance(&self) -> bool {
        self.parsed && self.verdict == ReviewVerdict::Accept
    }
}

/// Parse a review response into a verdict.
///
/// Accepts a small, explicit JSON shape. Anything else is `parsed: false`
/// with a Reject verdict — an unparseable review must never read as
/// approval.
pub fn parse_review(content: &str, identity: impl Into<String>) -> ReviewOutcome {
    let candidate = content.trim();
    let candidate = candidate
        .strip_prefix("```json")
        .and_then(|c| c.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(candidate);

    let parsed: Option<Value> = serde_json::from_str(candidate).ok();
    let Some(value) = parsed else {
        return ReviewOutcome {
            verdict: ReviewVerdict::Reject,
            concerns: vec!["the review was not valid JSON".to_owned()],
            parsed: false,
            identity: identity.into(),
        };
    };

    let verdict = match value.get("verdict").and_then(Value::as_str) {
        Some("accept") => ReviewVerdict::Accept,
        Some("concerns") => ReviewVerdict::Concerns,
        Some("reject") => ReviewVerdict::Reject,
        other => {
            return ReviewOutcome {
                verdict: ReviewVerdict::Reject,
                concerns: vec![format!("the review named an unknown verdict {other:?}")],
                parsed: false,
                identity: identity.into(),
            };
        }
    };

    let concerns: Vec<String> = value
        .get("concerns")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    // "accept" with concerns listed is a contradiction: treat the
    // concerns as authoritative rather than the label.
    let verdict = if verdict == ReviewVerdict::Accept && !concerns.is_empty() {
        ReviewVerdict::Concerns
    } else {
        verdict
    };

    ReviewOutcome {
        verdict,
        concerns,
        parsed: true,
        identity: identity.into(),
    }
}

/// Run a substantive review on the configured reasoner.
///
/// The review sees the task, the diff summary and the check evidence, and
/// is asked for a bounded JSON verdict. It runs on the reasoner tier: a
/// fast control-layer answer is not a substitute for code review.
pub async fn review_change(
    cascade: &crate::ComputeCascade,
    verifier: &dyn Verifier,
    task: &str,
    diff_summary: &str,
    checks: &[CheckEvidence],
) -> Result<ReviewOutcome, KnutError> {
    let check_summary: Vec<Value> = checks
        .iter()
        .map(|check| {
            json!({
                "check": check.name,
                "outcome": check.outcome,
                "reason": check.reason,
                "tests_passed": check.test_counts.passed,
                "tests_failed": check.test_counts.failed,
            })
        })
        .collect();

    let request = ModelRequest::new(
        "Review this change against the task. Answer with JSON \
         {\"verdict\": \"accept\"|\"concerns\"|\"reject\", \"concerns\": [\"...\"]}. \
         Judge only what the task asked for: constraints, unintended behavior, \
         scope and unexplained changes. Do not claim the tests prove correctness.",
        ExpectedArtifact::Json,
    )
    .with_input(json!({
        "task": task,
        "change": diff_summary,
        "checks": check_summary,
    }));

    let outcome = cascade
        .run(&request, crate::ModelTier::Reasoner, verifier)
        .await?;
    Ok(parse_review(
        &outcome.response.content,
        outcome.response.identity.model,
    ))
}

/// A verifier that accepts anything: only for tests and explicitly marked
/// fixtures, never for a real completion path.
///
/// This exists so the type is available to fixtures; the session's real
/// path never consults it.
pub struct AcceptAllVerifier;

impl Verifier for AcceptAllVerifier {
    fn verify(&self, _response: &ModelResponse) -> crate::VerificationVerdict {
        crate::VerificationVerdict::Sufficient
    }
}

/// Evidence gathered for one revision, with the gate's verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceReport {
    pub revision: ArtifactRevision,
    pub checks: Vec<CheckEvidence>,
    /// Whether every blocking requirement has fresh passing evidence.
    pub complete: bool,
    /// What is still outstanding, most useful first.
    pub outstanding: Vec<String>,
    /// Tests the change removed or disabled, if a comparison was made.
    pub weakened_tests: Vec<String>,
    pub review: Option<ReviewOutcome>,
}

impl EvidenceReport {
    /// Build a report from checks and the completion contract.
    pub fn build(
        requirements: &CompletionRequirements,
        revision: ArtifactRevision,
        checks: Vec<CheckEvidence>,
        weakened_tests: Vec<String>,
        review: Option<ReviewOutcome>,
    ) -> Self {
        let evidence: Vec<Evidence> = checks.iter().map(CheckEvidence::to_evidence).collect();
        let complete = requirements.satisfied(&evidence, &revision);
        let outstanding = requirements.missing(&evidence, &revision);
        Self {
            revision,
            checks,
            complete,
            outstanding,
            weakened_tests,
            review,
        }
    }

    /// Whether the report may be shown as a green result.
    pub fn is_green(&self) -> bool {
        self.complete && self.weakened_tests.is_empty()
    }

    /// A compact, human-readable summary for a review pane.
    pub fn summary(&self) -> String {
        let mut out = format!("revision {}\n", self.revision.revision);
        for check in &self.checks {
            out.push_str(&format!(
                "  {}: {:?} ({})",
                check.name, check.outcome, check.reason
            ));
            if check.test_counts.passed.is_some() || check.test_counts.failed.is_some() {
                out.push_str(&format!(
                    " [{} passed, {} failed]",
                    check.test_counts.passed.unwrap_or(0),
                    check.test_counts.failed.unwrap_or(0)
                ));
            }
            out.push('\n');
        }
        if !self.weakened_tests.is_empty() {
            out.push_str(&format!(
                "  weakened tests: {}\n",
                self.weakened_tests.join(", ")
            ));
        }
        if !self.outstanding.is_empty() {
            out.push_str(&format!("  outstanding: {}\n", self.outstanding.join("; ")));
        }
        if let Some(review) = &self.review {
            out.push_str(&format!(
                "  review: {:?} ({} concern(s))\n",
                review.verdict,
                review.concerns.len()
            ));
        }
        out
    }
}

/// Compare two check runs and flag a weakened acceptance set.
pub fn detect_weakened_tests(before: &CheckEvidence, after: &CheckEvidence) -> Vec<String> {
    let mut change =
        TestSetChange::compare(&test_names(&before.output), &test_names(&after.output));
    if change.removed.is_empty() && change.newly_ignored.is_empty() {
        // Fall back to counts when names are unavailable: a suite that
        // suddenly runs fewer tests is a weakening signal.
        if let (Some(was), Some(now)) = (before.test_counts.passed, after.test_counts.passed)
            && now < was
        {
            change
                .newly_ignored
                .push(format!("passed test count fell from {was} to {now}"));
        }
    }
    change
        .removed
        .into_iter()
        .chain(change.newly_ignored)
        .collect()
}

/// Extract test names from runner output (`test foo ... ok`).
fn test_names(output: &str) -> Vec<String> {
    let mut names = BTreeMap::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("test ") {
            let mut parts = rest.split(" ... ");
            let name = parts.next().unwrap_or_default().trim();
            let result = parts.next().unwrap_or_default().trim();
            if name.is_empty() {
                continue;
            }
            let ignored = result.starts_with("ignored");
            let key = if ignored {
                format!("ignored:{name}")
            } else {
                name.to_owned()
            };
            names.insert(key, ());
        }
    }
    names.into_keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "knut-checks-{name}-{}-{:?}",
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

    fn runner(fixture: &Fixture, profile: CheckProfile) -> CheckRunner {
        let supervisor = Arc::new(Supervisor::new(fixture.workspace()));
        CheckRunner::new(fixture.workspace(), supervisor, profile)
    }

    fn sh_check(name: &str, script: &str) -> CheckSpec {
        CheckSpec::new(
            name,
            "fixture check",
            "/bin/sh",
            vec!["-c".to_owned(), script.to_owned()],
        )
        .with_writable(vec![".".to_owned()])
    }

    #[tokio::test]
    async fn a_passing_check_produces_revision_bound_evidence() {
        let fixture = Fixture::new("pass");
        fixture.write("marker.txt", "x\n");
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![sh_check("build", "echo BUILD OK; exit 0")],
            },
        );

        let revision = ArtifactRevision::new("src", "rev-1");
        let evidence = runner.run_all(&revision).await;

        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].outcome, CheckOutcome::Passed);
        assert_eq!(evidence[0].revision, revision);
        assert_eq!(evidence[0].command[0], "/bin/sh");
        assert!(evidence[0].output.contains("BUILD OK"));

        let projected = evidence[0].to_evidence();
        assert!(projected.passed);
        assert_eq!(projected.subject, revision);
    }

    #[tokio::test]
    async fn a_failing_check_is_failed_not_green() {
        let fixture = Fixture::new("fail");
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![sh_check(
                    "test",
                    "echo 'test result: FAILED. 1 passed; 1 failed'; exit 101",
                )],
            },
        );

        let evidence = runner.run_all(&ArtifactRevision::new("src", "rev-1")).await;
        assert_eq!(evidence[0].outcome, CheckOutcome::Failed);
        assert_eq!(evidence[0].exit_code, Some(101));
        assert_eq!(evidence[0].test_counts.failed, Some(1));
        assert!(!evidence[0].to_evidence().passed);
    }

    #[tokio::test]
    async fn empty_test_discovery_is_not_a_pass() {
        let fixture = Fixture::new("empty");
        // A test runner that exits 0 having run nothing: exactly the case
        // that must not look green.
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![sh_check(
                    "test",
                    "echo 'running 0 tests'; echo 'test result: ok. 0 passed; 0 failed; 0 ignored'; exit 0",
                )],
            },
        );

        let evidence = runner.run_all(&ArtifactRevision::new("src", "rev-1")).await;
        assert_eq!(evidence[0].outcome, CheckOutcome::Inconclusive);
        assert!(evidence[0].reason.contains("no tests"));
        assert!(!evidence[0].to_evidence().passed);
    }

    #[tokio::test]
    async fn a_timeout_is_inconclusive_and_never_passes() {
        let fixture = Fixture::new("timeout");
        let mut check = sh_check("test", "sleep 60");
        check.timeout_secs = 1;
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![check],
            },
        );

        let evidence = runner.run_all(&ArtifactRevision::new("src", "rev-1")).await;
        assert_eq!(evidence[0].outcome, CheckOutcome::Inconclusive);
        assert!(evidence[0].reason.contains("time limit"));
        assert!(!evidence[0].to_evidence().passed);
    }

    #[tokio::test]
    async fn malformed_output_that_claims_success_is_inconclusive() {
        let fixture = Fixture::new("malformed");
        // Exit 0, but the runner's expected summary line is missing.
        let mut check = sh_check("test", "echo 'garbage output'; exit 0");
        check.expect_output_contains = vec!["test result:".to_owned()];
        check.require_output_marker = true;
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![check],
            },
        );

        let evidence = runner.run_all(&ArtifactRevision::new("src", "rev-1")).await;
        assert_eq!(evidence[0].outcome, CheckOutcome::Inconclusive);
        assert!(evidence[0].reason.contains("expected output"));
    }

    #[tokio::test]
    async fn an_unavailable_tool_is_reported_not_skipped_quietly() {
        let fixture = Fixture::new("missing");
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![CheckSpec::new(
                    "typecheck",
                    "types",
                    "definitely-not-installed-xyz",
                    vec![],
                )],
            },
        );

        let evidence = runner.run_all(&ArtifactRevision::new("src", "rev-1")).await;
        assert_eq!(evidence[0].outcome, CheckOutcome::Unavailable);
        assert!(evidence[0].reason.contains("not installed"));
    }

    #[tokio::test]
    async fn test_passing_on_revision_a_does_not_validate_revision_b() {
        let fixture = Fixture::new("revisions");
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![sh_check(
                    "test",
                    "echo 'test result: ok. 5 passed; 0 failed'; exit 0",
                )],
            },
        );

        let revision_a = ArtifactRevision::new("src", "rev-a");
        let evidence_a = runner.run_all(&revision_a).await;
        assert!(evidence_a[0].outcome.is_green());

        // The file changes; the old evidence is now stale, because the
        // requirement is checked against the *current* revision.
        fixture.write("src/lib.rs", "fn main() {}\n");
        let revision_b = runner.current_revision("src").unwrap();
        assert_ne!(revision_a, revision_b);

        let requirements = runner.requirements();
        let report = EvidenceReport::build(
            &requirements,
            revision_b.clone(),
            evidence_a,
            Vec::new(),
            None,
        );
        assert!(!report.complete, "stale evidence satisfied a new revision");
        assert!(!report.outstanding.is_empty());
    }

    #[tokio::test]
    async fn a_fixture_patch_that_compiles_but_fails_a_regression_test_cannot_complete() {
        let fixture = Fixture::new("regression");
        fixture.write("src/lib.rs", "// fix\n");
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![
                    sh_check("build", "exit 0"),
                    sh_check(
                        "test",
                        "echo 'test result: FAILED. 3 passed; 1 failed'; exit 101",
                    ),
                ],
            },
        );

        let revision = runner.current_revision("src").unwrap();
        let checks = runner.run_all(&revision).await;
        let report =
            EvidenceReport::build(&runner.requirements(), revision, checks, Vec::new(), None);

        assert!(!report.complete);
        assert!(!report.is_green());
        // The build passed and the test did not: both facts are visible.
        assert_eq!(report.checks[0].outcome, CheckOutcome::Passed);
        assert_eq!(report.checks[1].outcome, CheckOutcome::Failed);
        assert!(report.summary().contains("test: Failed"));
    }

    #[tokio::test]
    async fn a_valid_patch_completes_with_inspectable_evidence() {
        let fixture = Fixture::new("valid");
        fixture.write("src/lib.rs", "fn main() {}\n");
        let runner = runner(
            &fixture,
            CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![
                    sh_check("build", "echo 'compiled'; exit 0"),
                    sh_check(
                        "test",
                        "echo 'test result: ok. 7 passed; 0 failed; 0 ignored'; exit 0",
                    ),
                    // Advisory: a lint warning must not block a correct fix.
                    sh_check("lint", "echo 'warning: style'; exit 0"),
                ],
            },
        );

        let revision = runner.current_revision("src").unwrap();
        let checks = runner.run_all(&revision).await;
        let report = EvidenceReport::build(
            &runner.requirements(),
            revision.clone(),
            checks,
            Vec::new(),
            None,
        );

        assert!(report.complete, "outstanding: {:?}", report.outstanding);
        assert!(report.is_green());
        // Every check is inspectable with its command and revision.
        for check in &report.checks {
            assert_eq!(check.revision, revision);
            assert!(!check.command.is_empty());
        }
        assert!(report.summary().contains("7 passed"));
    }

    #[test]
    fn deleted_or_disabled_tests_are_flagged() {
        let before = vec![
            "test_a".to_owned(),
            "test_b".to_owned(),
            "test_c".to_owned(),
        ];
        let after = vec!["test_a".to_owned(), "ignored:test_c".to_owned()];
        let change = TestSetChange::compare(&before, &after);

        assert_eq!(change.removed, vec!["test_b".to_owned()]);
        assert_eq!(change.newly_ignored, vec!["test_c".to_owned()]);
        assert!(change.weakens());
    }

    #[test]
    fn weakened_tests_block_a_green_report() {
        let revision = ArtifactRevision::new("src", "rev");
        let check = CheckEvidence {
            name: "test".to_owned(),
            description: "tests".to_owned(),
            outcome: CheckOutcome::Passed,
            revision: revision.clone(),
            command: vec!["cargo".to_owned(), "test".to_owned()],
            exit_code: Some(0),
            signal: None,
            duration_ms: 10,
            output: "test result: ok. 2 passed; 0 failed".to_owned(),
            output_truncated: false,
            test_counts: TestCounts {
                passed: Some(2),
                failed: Some(0),
                ignored: Some(0),
            },
            reason: "passed".to_owned(),
        };
        let requirements = CompletionRequirements::none().require("test", "tests pass", true);
        let report = EvidenceReport::build(
            &requirements,
            revision,
            vec![check],
            vec!["test_c".to_owned()],
            None,
        );

        assert!(report.complete);
        // Green requires both fresh evidence *and* an intact test set.
        assert!(!report.is_green());
    }

    #[test]
    fn weakened_detection_uses_counts_when_names_are_unavailable() {
        let before = CheckEvidence {
            name: "test".to_owned(),
            description: String::new(),
            outcome: CheckOutcome::Passed,
            revision: ArtifactRevision::new("s", "1"),
            command: vec![],
            exit_code: Some(0),
            signal: None,
            duration_ms: 1,
            output: String::new(),
            output_truncated: false,
            test_counts: TestCounts {
                passed: Some(10),
                failed: Some(0),
                ignored: Some(0),
            },
            reason: String::new(),
        };
        let mut after = before.clone();
        after.test_counts.passed = Some(4);

        let weakened = detect_weakened_tests(&before, &after);
        assert!(!weakened.is_empty());
    }

    #[test]
    fn an_unparseable_review_is_never_an_acceptance() {
        let outcome = parse_review("I think it looks fine to me!", "model");
        assert!(!outcome.parsed);
        assert!(!outcome.is_acceptance());
        assert_eq!(outcome.verdict, ReviewVerdict::Reject);

        let unknown = parse_review(r#"{"verdict": "looks_good"}"#, "model");
        assert!(!unknown.parsed);
        assert!(!unknown.is_acceptance());
    }

    #[test]
    fn review_parsing_handles_verdicts_and_concerns() {
        let accept = parse_review(r#"{"verdict":"accept","concerns":[]}"#, "m");
        assert!(accept.is_acceptance());

        // Accept-with-concerns is a contradiction: the concerns win.
        let contradictory = parse_review(
            r#"{"verdict":"accept","concerns":["touches unrelated code"]}"#,
            "m",
        );
        assert_eq!(contradictory.verdict, ReviewVerdict::Concerns);
        assert!(!contradictory.is_acceptance());

        let reject = parse_review(
            r#"{"verdict":"reject","concerns":["does not fix the reported bug"]}"#,
            "m",
        );
        assert_eq!(reject.verdict, ReviewVerdict::Reject);
        assert!(reject.parsed);
    }

    #[test]
    fn profiles_are_discovered_from_workspace_evidence() {
        let fixture = Fixture::new("discover");
        // An empty workspace suggests nothing.
        assert!(discover_profiles(&fixture.workspace()).is_empty());

        // A Cargo.toml suggests rust; a package.json suggests typescript.
        fixture.write("Cargo.toml", "[package]\nname = \"x\"\n");
        let profiles = discover_profiles(&fixture.workspace());
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "rust");

        fixture.write("package.json", "{}\n");
        let profiles = discover_profiles(&fixture.workspace());
        assert_eq!(profiles.len(), 2);
    }

    #[test]
    fn rust_profile_gates_on_build_and_test_not_lint() {
        let profile = CheckProfile::rust();
        let supervisor = Arc::new(Supervisor::new(Fixture::new("requirements").workspace()));
        let runner = CheckRunner::new(
            Workspace::open("/tmp").unwrap_or_else(|_| Workspace::open(".").unwrap()),
            supervisor,
            profile,
        );
        let requirements = runner.requirements();
        let blocking: Vec<&str> = requirements
            .requirements()
            .iter()
            .filter(|r| r.blocking)
            .map(|r| r.check.as_str())
            .collect();
        assert!(blocking.contains(&"build"));
        assert!(blocking.contains(&"test"));
        assert!(!blocking.contains(&"lint"));
    }

    #[test]
    fn counts_aggregate_across_test_binaries() {
        // A real `cargo test` prints one summary per binary: the totals
        // must add up, or a passing suite looks like it ran nothing.
        let output = "running 245 tests\n\
                      test result: ok. 245 passed; 0 failed; 0 ignored; 0 measured\n\n\
                      running 0 tests\n\n\
                      test result: ok. 0 passed; 0 failed; 0 ignored\n";
        let counts = TestCounts::parse(output);
        assert_eq!(counts.passed, Some(245));
        assert_eq!(counts.failed, Some(0));
        assert!(counts.ran_any());

        // Nothing reported at all stays unknown.
        let silent = TestCounts::parse("no summary lines here");
        assert_eq!(silent.passed, None);
        assert!(!silent.ran_any());
    }

    #[test]
    fn test_counts_parse_cargo_and_jest_shapes() {
        let cargo =
            TestCounts::parse("test result: ok. 12 passed; 0 failed; 1 ignored; 0 measured");
        assert_eq!(cargo.passed, Some(12));
        assert_eq!(cargo.failed, Some(0));
        assert_eq!(cargo.ignored, Some(1));
        assert!(cargo.ran_any());

        let jest = TestCounts::parse("Tests:       3 failed, 4 passed, 7 total");
        assert_eq!(jest.passed, Some(4));
        assert_eq!(jest.failed, Some(3));

        let none = TestCounts::parse("nothing ran here");
        assert!(!none.ran_any());
    }
}
