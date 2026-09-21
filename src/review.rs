//! The review workspace: diffs, hunk selection, approvals and check
//! evidence in one view (issue #29).
//!
//! Review is the centre of the coding loop here, so the pieces the user
//! needs are computed and presented together: what will change, exactly
//! what is being approved, what has actually been checked against which
//! revision, and what remains unresolved.
//!
//! Two rules carry most of the weight:
//! - **selecting a subset is a new proposal.** Rejecting a hunk produces a
//!   different normalized patch with a different identity, so an earlier
//!   approval — and any check evidence bound to the old revision — cannot
//!   silently authorize the revised change.
//! - **an approval cannot capture a keystroke.** The view tracks a
//!   monotonic approval generation, and a prompt that appears while the
//!   user is typing is not allowed to consume a key meant for the
//!   composer.

use std::collections::BTreeMap;

use crate::patch::{Patch, PatchOp, ValidatedPatch};
use crate::verify::{CheckEvidence, CheckOutcome};
use crate::workspace::content_hash;

/// One file in the changes view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Workspace-relative path (the new path for a rename).
    pub path: String,
    /// Previous path for a rename.
    pub from: Option<String>,
    pub kind: ChangeKind,
    /// Unified-diff hunks for this file.
    pub hunks: Vec<Hunk>,
    /// Lines added/removed, for the summary.
    pub added: usize,
    pub removed: usize,
}

/// What happened to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Created => "created",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
        }
    }

    pub fn marker(self) -> &'static str {
        match self {
            ChangeKind::Created => "+",
            ChangeKind::Modified => "~",
            ChangeKind::Deleted => "-",
            ChangeKind::Renamed => ">",
        }
    }
}

/// One hunk of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Stable id within the change set: `<path>#<ordinal>`.
    pub id: String,
    /// 1-based starting line in the original file.
    pub old_start: usize,
    /// 1-based starting line in the new file.
    pub new_start: usize,
    /// Diff lines including their leading marker (` `, `-`, `+`).
    pub lines: Vec<String>,
    pub added: usize,
    pub removed: usize,
}

impl Hunk {
    /// A one-line header for the UI.
    pub fn header(&self) -> String {
        format!(
            "@@ -{} +{} @@ (+{} -{})",
            self.old_start, self.new_start, self.added, self.removed
        )
    }
}

/// Maximum diff lines rendered per hunk before the view folds it.
pub const MAX_HUNK_LINES: usize = 400;
/// Maximum files listed before the view summarises the remainder.
pub const MAX_FILES: usize = 200;

/// The whole change set under review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    pub files: Vec<FileChange>,
    /// Identity of the proposal this change set came from.
    pub proposal_identity: String,
    /// Revisions of the files the proposal depends on, so a concurrent
    /// edit can be detected without re-reading anything.
    pub preconditions: Vec<(String, String)>,
}

impl ChangeSet {
    /// Build a change set from a validated patch and the current file
    /// contents.
    pub fn from_patch(patch: &ValidatedPatch, contents: &BTreeMap<String, String>) -> Self {
        let mut files = Vec::new();
        for (index, op) in patch.ops.iter().enumerate() {
            let before = op
                .from
                .as_ref()
                .and_then(|from| contents.get(from))
                .or_else(|| contents.get(&op.path))
                .cloned()
                .unwrap_or_default();
            let after = op
                .content
                .clone()
                .or_else(|| contents.get(&op.path).cloned())
                .unwrap_or_default();

            let kind = match op.kind.as_str() {
                "create" => ChangeKind::Created,
                "delete" => ChangeKind::Deleted,
                "rename" => ChangeKind::Renamed,
                _ => ChangeKind::Modified,
            };

            let hunks = match kind {
                // A delete removes everything; a create adds everything.
                ChangeKind::Deleted => diff_hunks(&op.path, &before, "", index),
                ChangeKind::Created | ChangeKind::Modified | ChangeKind::Renamed => {
                    diff_hunks(&op.path, &before, &after, index)
                }
            };
            let added = hunks.iter().map(|hunk| hunk.added).sum();
            let removed = hunks.iter().map(|hunk| hunk.removed).sum();

            files.push(FileChange {
                path: op.path.clone(),
                from: op.from.clone(),
                kind,
                hunks,
                added,
                removed,
            });
        }
        files.truncate(MAX_FILES);

        Self {
            files,
            proposal_identity: patch.identity.clone(),
            preconditions: patch.preconditions.clone(),
        }
    }

    /// Total added/removed across the change set.
    pub fn totals(&self) -> (usize, usize) {
        (
            self.files.iter().map(|file| file.added).sum(),
            self.files.iter().map(|file| file.removed).sum(),
        )
    }

    /// Every hunk id, in view order.
    pub fn hunk_ids(&self) -> Vec<String> {
        self.files
            .iter()
            .flat_map(|file| file.hunks.iter().map(|hunk| hunk.id.clone()))
            .collect()
    }

    /// Find a file by path.
    pub fn file(&self, path: &str) -> Option<&FileChange> {
        self.files.iter().find(|file| file.path == path)
    }
}

/// Produce unified-diff hunks between two texts.
///
/// A plain line-based diff with context. It is deliberately simple and
/// complete rather than clever: the review view shows what changed, and a
/// patch that no longer applies is reported as stale instead of being
/// fuzzy-repaired.
pub fn diff_hunks(path: &str, before: &str, after: &str, ordinal: usize) -> Vec<Hunk> {
    let old_lines: Vec<&str> = before.lines().collect();
    let new_lines: Vec<&str> = after.lines().collect();

    // Longest common subsequence over lines, bounded so a pathological
    // input cannot blow up memory: the review view is not a merge engine.
    let max_cells = 4_000_000usize;
    if old_lines.len().saturating_mul(new_lines.len()) > max_cells {
        return vec![Hunk {
            id: format!("{path}#{ordinal}.0"),
            old_start: 1,
            new_start: 1,
            lines: vec![format!(
                "  ({} -> {} lines; diff too large to render inline)",
                old_lines.len(),
                new_lines.len()
            )],
            added: new_lines.len(),
            removed: old_lines.len(),
        }];
    }

    // Standard LCS table.
    let mut table = vec![vec![0u32; new_lines.len() + 1]; old_lines.len() + 1];
    for i in (0..old_lines.len()).rev() {
        for j in (0..new_lines.len()).rev() {
            table[i][j] = if old_lines[i] == new_lines[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    // Walk the table, emitting context/removed/added lines.
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < old_lines.len() && j < new_lines.len() {
        if old_lines[i] == new_lines[j] {
            ops.push((' ', old_lines[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            ops.push(('-', old_lines[i]));
            i += 1;
        } else {
            ops.push(('+', new_lines[j]));
            j += 1;
        }
    }
    while i < old_lines.len() {
        ops.push(('-', old_lines[i]));
        i += 1;
    }
    while j < new_lines.len() {
        ops.push(('+', new_lines[j]));
        j += 1;
    }

    // Group into hunks with a context window around each change.
    const CONTEXT: usize = 3;
    let mut hunks = Vec::new();
    let mut index = 0usize;
    let mut ordinal_in_file = 0usize;
    while index < ops.len() {
        if ops[index].0 == ' ' {
            index += 1;
            continue;
        }
        // Start a hunk CONTEXT lines earlier.
        let start = index.saturating_sub(CONTEXT);
        let mut end = index;
        let mut since_change = 0usize;
        while end < ops.len() {
            match ops[end].0 {
                ' ' => {
                    since_change += 1;
                    if since_change > CONTEXT * 2 {
                        break;
                    }
                }
                _ => since_change = 0,
            }
            end += 1;
        }

        let slice = &ops[start..end];
        let lines: Vec<String> = slice
            .iter()
            .map(|(marker, text)| format!("{marker}{text}"))
            .collect();
        let added = slice.iter().filter(|(m, _)| *m == '+').count();
        let removed = slice.iter().filter(|(m, _)| *m == '-').count();

        // Line numbers of the hunk's first line in each file.
        let old_start = ops[..start].iter().filter(|(m, _)| *m != '+').count() + 1;
        let new_start = ops[..start].iter().filter(|(m, _)| *m != '-').count() + 1;

        hunks.push(Hunk {
            id: format!("{path}#{ordinal}.{ordinal_in_file}"),
            old_start,
            new_start,
            lines,
            added,
            removed,
        });
        ordinal_in_file += 1;
        index = end.max(index + 1);
    }

    hunks
}

/// Which hunks the user kept.
///
/// Default is "all kept"; rejecting one produces a different proposal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HunkSelection {
    /// Rejected hunk ids.
    rejected: Vec<String>,
}

impl HunkSelection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rejected(&self) -> &[String] {
        &self.rejected
    }

    pub fn is_rejected(&self, id: &str) -> bool {
        self.rejected.iter().any(|existing| existing == id)
    }

    /// Reject a hunk (or keep it again).
    pub fn toggle(&mut self, id: &str) {
        if let Some(position) = self.rejected.iter().position(|existing| existing == id) {
            self.rejected.remove(position);
        } else {
            self.rejected.push(id.to_owned());
        }
    }

    pub fn reject(&mut self, id: &str) {
        if !self.is_rejected(id) {
            self.rejected.push(id.to_owned());
        }
    }

    pub fn keep(&mut self, id: &str) {
        self.rejected.retain(|existing| existing != id);
    }

    pub fn clear(&mut self) {
        self.rejected.clear();
    }
}

/// The result of applying a hunk selection to a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisedProposal {
    pub patch: Patch,
    /// Identity of the revised proposal — different from the original
    /// whenever anything was rejected.
    pub identity: String,
    /// Whether this differs from the proposal it came from.
    pub changed: bool,
}

/// Build the revised patch for a selection.
///
/// Rejecting *part* of a file's change is represented by re-deriving that
/// file's content without the rejected hunks: the result is a single
/// whole-file replacement with a new precondition, so the identity and
/// the approval fingerprint both change.
pub fn revise_proposal(
    original: &Patch,
    selection: &HunkSelection,
    contents: &BTreeMap<String, String>,
) -> RevisedProposal {
    if selection.rejected().is_empty() {
        return RevisedProposal {
            patch: original.clone(),
            identity: original.identity(),
            changed: false,
        };
    }

    let mut ops = Vec::new();
    for op in &original.ops {
        let path = op.path();
        let rejected_for_path: Vec<&String> = selection
            .rejected()
            .iter()
            .filter(|id| id.starts_with(&format!("{path}#")))
            .collect();

        if rejected_for_path.is_empty() {
            ops.push(op.clone());
            continue;
        }

        match op {
            PatchOp::Create { path, content } => {
                // Rejecting the only hunk of a creation means the file is
                // not created at all.
                let hunks = diff_hunks(path, "", content, 0);
                if hunks.iter().all(|hunk| selection.is_rejected(&hunk.id)) {
                    continue;
                }
                ops.push(op.clone());
            }
            PatchOp::Replace {
                path,
                content,
                expect_hash,
            } => {
                let before = contents.get(path).cloned().unwrap_or_default();
                let hunks = diff_hunks(path, &before, content, 0);
                let kept: Vec<&Hunk> = hunks
                    .iter()
                    .filter(|hunk| !selection.is_rejected(&hunk.id))
                    .collect();
                if kept.is_empty() {
                    continue;
                }
                if kept.len() == hunks.len() {
                    ops.push(op.clone());
                    continue;
                }
                // Keep only the accepted hunks by reconstructing the file.
                let revised = apply_kept_hunks(&before, &hunks, &kept);
                ops.push(PatchOp::Replace {
                    path: path.clone(),
                    content: revised,
                    expect_hash: expect_hash.clone(),
                });
            }
            PatchOp::Delete { .. } | PatchOp::Rename { .. } => {
                // A delete or a move is one indivisible operation: the
                // user either accepts it or the file is left alone.
                let hunks = match op {
                    PatchOp::Delete { path, .. } => {
                        let before = contents.get(path).cloned().unwrap_or_default();
                        diff_hunks(path, &before, "", 0)
                    }
                    PatchOp::Rename { from, to, .. } => {
                        let before = contents.get(from).cloned().unwrap_or_default();
                        diff_hunks(to, &before, &before, 0)
                    }
                    _ => unreachable!(),
                };
                if hunks.iter().all(|hunk| selection.is_rejected(&hunk.id)) {
                    continue;
                }
                ops.push(op.clone());
            }
        }
    }

    let patch = Patch::new(original.summary.clone(), ops);
    let identity = patch.identity();
    RevisedProposal {
        changed: identity != original.identity(),
        patch,
        identity,
    }
}

/// Rebuild file content from the original, applying only the kept hunks.
fn apply_kept_hunks(before: &str, hunks: &[Hunk], kept: &[&Hunk]) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut cursor = 0usize; // index into before_lines

    for hunk in hunks {
        let accepted = kept.iter().any(|k| k.id == hunk.id);
        // Context lines before this hunk's first change.
        // Context lines are copied verbatim for a kept or a rejected
        // hunk: only added/removed lines depend on the decision.
        let hunk_old_start = hunk.old_start.saturating_sub(1);
        while cursor < hunk_old_start && cursor < before_lines.len() {
            result.push(before_lines[cursor].to_owned());
            cursor += 1;
        }

        for line in &hunk.lines {
            let (marker, text) = line.split_at(1);
            match marker {
                " " => {
                    if cursor < before_lines.len() {
                        result.push(before_lines[cursor].to_owned());
                        cursor += 1;
                    } else {
                        result.push(text.to_owned());
                    }
                }
                "-" => {
                    // Removed line: kept hunks drop it, rejected hunks
                    // restore the original text.
                    if !accepted {
                        result.push(text.to_owned());
                    }
                    cursor += 1;
                }
                // An added line is only in the result when the hunk was
                // kept.
                "+" if accepted => result.push(text.to_owned()),
                _ => {}
            }
        }
    }

    // Remaining lines after the last hunk.
    while cursor < before_lines.len() {
        result.push(before_lines[cursor].to_owned());
        cursor += 1;
    }

    if result.is_empty() {
        return String::new();
    }
    let mut text = result.join("\n");
    // Preserve a trailing newline when the original had one and any line
    // survived.
    if before.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// An approval request shown in the review view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalView {
    /// Exact action being approved: command, arguments or a diff.
    pub action: String,
    /// Workspace the action runs against.
    pub workspace: String,
    /// Why approval is required.
    pub reason: String,
    /// Filesystem scope the action may touch.
    pub scope: Vec<String>,
    /// Whether the action may use the network.
    pub network: bool,
    /// The exact fingerprint the approval key names.
    pub approval_key: String,
    /// Monotonic generation: a view is only answerable if it is current.
    pub generation: u64,
}

/// What the user chose for an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    /// Approve this exact action, once.
    ApproveOnce,
    /// Reject it.
    Reject,
    /// Broader permission: a separate, explicit decision.
    AllowThisSession,
}

/// Why an approval could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    /// The view is stale: a newer request or revision superseded it.
    Stale { generation: u64, current: u64 },
    /// The action's precondition changed.
    Conflicts { path: String },
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApprovalError::Stale {
                generation,
                current,
            } => write!(
                f,
                "this approval (generation {generation}) is stale; the current one is {current}"
            ),
            ApprovalError::Conflicts { path } => {
                write!(f, "{path:?} changed since this approval was shown")
            }
        }
    }
}

/// The pending-approval state, with the guard against keystroke capture.
///
/// A prompt that appears while the user is typing belongs to the *next*
/// key press, not the one already in flight: the generation counter makes
/// that explicit, and a stale view cannot be answered at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalState {
    pending: Option<ApprovalView>,
    generation: u64,
    /// Set when a request arrived while the composer had unsent input.
    arrived_during_typing: bool,
}

impl ApprovalState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new approval request, returning its generation.
    pub fn present(&mut self, mut view: ApprovalView, composer_dirty: bool) -> u64 {
        self.generation += 1;
        view.generation = self.generation;
        self.arrived_during_typing = composer_dirty;
        self.pending = Some(view);
        self.generation
    }

    pub fn pending(&self) -> Option<&ApprovalView> {
        self.pending.as_ref()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the newest request arrived while the user was typing.
    pub fn arrived_during_typing(&self) -> bool {
        self.arrived_during_typing
    }

    /// Whether a keystroke may be interpreted as an approval decision.
    ///
    /// While the composer holds unsent text, approve/deny shortcuts are
    /// *not* armed: the user is typing a prompt, and a prompt appearing
    /// must not swallow that keystroke.
    pub fn shortcuts_armed(&self, composer_dirty: bool) -> bool {
        self.pending.is_some() && !composer_dirty
    }

    /// Answer the approval, refusing a stale view.
    pub fn answer(
        &mut self,
        generation: u64,
        current_hashes: &BTreeMap<String, String>,
        preconditions: &[(String, String)],
    ) -> Result<ApprovalChoice, ApprovalError> {
        if generation != self.generation {
            return Err(ApprovalError::Stale {
                generation,
                current: self.generation,
            });
        }
        // Re-check the preconditions: a concurrent edit invalidates the
        // proposal this approval was shown for.
        for (path, expected) in preconditions {
            let current = current_hashes.get(path);
            let actual = current
                .map(|content| content_hash(content.as_bytes()))
                .unwrap_or_else(|| "<missing>".to_owned());
            if actual != *expected {
                return Err(ApprovalError::Conflicts { path: path.clone() });
            }
        }
        self.pending = None;
        Ok(ApprovalChoice::ApproveOnce)
    }

    /// Explicitly grant broader permission for the session.
    pub fn allow_session(&mut self, generation: u64) -> Result<(), ApprovalError> {
        if generation != self.generation {
            return Err(ApprovalError::Stale {
                generation,
                current: self.generation,
            });
        }
        self.pending = None;
        Ok(())
    }

    /// Clear the pending request (e.g. it was resolved elsewhere).
    pub fn clear(&mut self) {
        self.pending = None;
        self.arrived_during_typing = false;
    }
}

/// One check's presentation in the review view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRow {
    pub name: String,
    pub state: CheckOutcome,
    /// The revision the check ran against.
    pub revision: String,
    /// Whether that revision is the proposal's current one.
    pub current_revision: bool,
    pub summary: String,
    /// First few diagnostics lines, for the "jump to diagnostic" affordance.
    pub diagnostics: Vec<String>,
    pub output_truncated: bool,
}

impl CheckRow {
    /// Build a row from evidence and the proposal's revision.
    pub fn from_evidence(evidence: &CheckEvidence, current: &str) -> Self {
        let diagnostics: Vec<String> = evidence
            .output
            .lines()
            .filter(|line| {
                let lowered = line.to_lowercase();
                lowered.contains("error") || lowered.contains("failed")
            })
            .take(5)
            .map(|line| line.chars().take(200).collect())
            .collect();

        Self {
            name: evidence.name.clone(),
            state: evidence.outcome,
            revision: evidence.revision.revision.clone(),
            current_revision: evidence.revision.revision == current,
            summary: evidence.reason.clone(),
            diagnostics,
            output_truncated: evidence.output_truncated,
        }
    }

    /// Whether this row may be shown as passing.
    ///
    /// A stale revision is never green: the code changed since the check
    /// ran, so the result says nothing about the current proposal.
    pub fn is_green(&self) -> bool {
        self.state.is_green() && self.current_revision
    }

    /// The label shown to the user, never "passed" unless it is.
    pub fn label(&self) -> String {
        if self.current_revision {
            self.state_label().to_owned()
        } else {
            format!("stale ({})", self.state_label())
        }
    }

    fn state_label(&self) -> &'static str {
        match self.state {
            CheckOutcome::Passed => "passed",
            CheckOutcome::Failed => "failed",
            CheckOutcome::Skipped => "skipped",
            CheckOutcome::Unavailable => "unavailable",
            CheckOutcome::Stale => "stale",
            CheckOutcome::Inconclusive => "inconclusive",
        }
    }
}

/// What the user is doing in the review view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFocus {
    Files,
    Diff,
    Checks,
    Approval,
}

/// The review view's navigable state.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewView {
    pub changes: ChangeSet,
    pub selection: HunkSelection,
    pub focus: ReviewFocus,
    /// Index into `changes.files`.
    pub file_index: usize,
    /// Index into the focused file's hunks.
    pub hunk_index: usize,
    /// Index into `checks`.
    pub check_index: usize,
    pub checks: Vec<CheckRow>,
    pub approvals: ApprovalState,
    /// Whether side-by-side rendering is requested (only used when the
    /// terminal is wide enough).
    pub side_by_side: bool,
    /// Scroll offset within the diff pane.
    pub scroll: usize,
}

impl ReviewView {
    pub fn new(changes: ChangeSet) -> Self {
        Self {
            changes,
            selection: HunkSelection::new(),
            focus: ReviewFocus::Diff,
            file_index: 0,
            hunk_index: 0,
            check_index: 0,
            checks: Vec::new(),
            approvals: ApprovalState::new(),
            side_by_side: false,
            scroll: 0,
        }
    }

    pub fn current_file(&self) -> Option<&FileChange> {
        self.changes.files.get(self.file_index)
    }

    pub fn current_hunk(&self) -> Option<&Hunk> {
        self.current_file()
            .and_then(|file| file.hunks.get(self.hunk_index))
    }

    /// Move to the next file.
    pub fn next_file(&mut self) {
        if self.file_index + 1 < self.changes.files.len() {
            self.file_index += 1;
            self.hunk_index = 0;
            self.scroll = 0;
        }
    }

    /// Move to the previous file.
    pub fn previous_file(&mut self) {
        if self.file_index > 0 {
            self.file_index -= 1;
            self.hunk_index = 0;
            self.scroll = 0;
        }
    }

    /// Move to the next hunk, crossing into the next file.
    pub fn next_hunk(&mut self) {
        let hunk_count = self
            .current_file()
            .map(|file| file.hunks.len())
            .unwrap_or(0);
        if self.hunk_index + 1 < hunk_count {
            self.hunk_index += 1;
        } else if self.file_index + 1 < self.changes.files.len() {
            self.file_index += 1;
            self.hunk_index = 0;
        }
        self.scroll = 0;
    }

    /// Move to the previous hunk, crossing into the previous file.
    pub fn previous_hunk(&mut self) {
        if self.hunk_index > 0 {
            self.hunk_index -= 1;
        } else if self.file_index > 0 {
            self.file_index -= 1;
            self.hunk_index = self
                .current_file()
                .map(|file| file.hunks.len().saturating_sub(1))
                .unwrap_or(0);
        }
        self.scroll = 0;
    }

    /// Reject the hunk under the cursor.
    pub fn reject_current_hunk(&mut self) -> Option<String> {
        let id = self.current_hunk()?.id.clone();
        self.selection.reject(&id);
        Some(id)
    }

    /// Keep the hunk under the cursor again.
    pub fn keep_current_hunk(&mut self) -> Option<String> {
        let id = self.current_hunk()?.id.clone();
        self.selection.keep(&id);
        Some(id)
    }

    /// The proposal the current selection represents.
    pub fn revised(
        &self,
        original: &Patch,
        contents: &BTreeMap<String, String>,
    ) -> RevisedProposal {
        revise_proposal(original, &self.selection, contents)
    }

    /// A summary of the completed work: files, checks and limitations.
    pub fn summary(&self) -> String {
        let (added, removed) = self.changes.totals();
        let mut out = format!(
            "{} file(s) changed, +{added} -{removed}\\n",
            self.changes.files.len()
        );
        for file in &self.changes.files {
            let from = file
                .from
                .as_ref()
                .map(|from| format!("{from} -> "))
                .unwrap_or_default();
            let rejected = file
                .hunks
                .iter()
                .filter(|hunk| self.selection.is_rejected(&hunk.id))
                .count();
            let rejected = if rejected > 0 {
                format!(" ({rejected} hunk(s) rejected)")
            } else {
                String::new()
            };
            out.push_str(&format!(
                "  {} {}{} +{} -{}{rejected}\\n",
                file.kind.marker(),
                from,
                file.path,
                file.added,
                file.removed
            ));
        }

        out.push_str("checks actually run:\\n");
        if self.checks.is_empty() {
            out.push_str("  none\\n");
        }
        for check in &self.checks {
            out.push_str(&format!(
                "  {}: {}{}\\n",
                check.name,
                check.label(),
                if check.summary.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", check.summary)
                }
            ));
        }

        // Limitations are stated, not implied away.
        let unresolved: Vec<String> = self
            .checks
            .iter()
            .filter(|check| !check.is_green())
            .map(|check| format!("{}: {}", check.name, check.label()))
            .collect();
        if unresolved.is_empty() {
            out.push_str("limitations: none recorded\\n");
        } else {
            out.push_str(&format!("limitations: {}\\n", unresolved.join("; ")));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::{PatchOp, validate_patch};
    use crate::workspace::Workspace;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "knut-review-{name}-{}-{:?}",
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

    /// A two-file proposal: one edit with two distant changes, plus a new
    /// file. Mirrors the acceptance test's "two files, reject a hunk".
    fn two_file_proposal(fixture: &mut Fixture) -> (Patch, BTreeMap<String, String>) {
        let original = (1..=40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fixture.write("src/a.rs", &original);

        // Two separated edits: hunk 0 near the top, hunk 1 near the bottom.
        let mut changed: Vec<String> = original.lines().map(str::to_owned).collect();
        changed[2] = "LINE 3 CHANGED".to_owned();
        changed[35] = "LINE 36 CHANGED".to_owned();
        let changed = changed.join("\n") + "\n";

        let hash = crate::workspace::content_hash(original.as_bytes());
        let patch = Patch::new(
            "edit two files",
            vec![
                PatchOp::Replace {
                    path: "src/a.rs".to_owned(),
                    content: changed,
                    expect_hash: hash,
                },
                PatchOp::Create {
                    path: "src/b.rs".to_owned(),
                    content: "pub fn new_thing() {}\\n".to_owned(),
                },
            ],
        );

        let mut contents = BTreeMap::new();
        contents.insert("src/a.rs".to_owned(), original);
        (patch, contents)
    }

    #[test]
    fn the_change_set_lists_files_and_hunks() {
        let mut fixture = Fixture::new("changes");
        let (patch, contents) = two_file_proposal(&mut fixture);
        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();

        let changes = ChangeSet::from_patch(&validated, &contents);
        assert_eq!(changes.files.len(), 2);
        assert_eq!(changes.files[0].kind, ChangeKind::Modified);
        assert_eq!(changes.files[1].kind, ChangeKind::Created);
        // Two distant edits are two hunks, not one giant diff.
        assert_eq!(changes.files[0].hunks.len(), 2);
        assert!(changes.files[0].added >= 2);
        assert!(changes.files[1].added >= 1);

        let (added, removed) = changes.totals();
        assert!(added > 0);
        assert_eq!(
            removed,
            changes.files[0]
                .hunks
                .iter()
                .map(|h| h.removed)
                .sum::<usize>()
        );
    }

    #[test]
    fn rejecting_a_hunk_produces_a_different_proposal_identity() {
        let mut fixture = Fixture::new("revise");
        let (patch, contents) = two_file_proposal(&mut fixture);
        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let changes = ChangeSet::from_patch(&validated, &contents);

        let mut view = ReviewView::new(changes);
        // Reject the second hunk of the first file.
        view.file_index = 0;
        view.hunk_index = 1;
        let rejected = view.reject_current_hunk().unwrap();
        assert!(rejected.contains("src/a.rs"));

        let revised = view.revised(&patch, &contents);
        // A subset selection is a *new* proposal: the identity differs, so
        // an earlier approval cannot authorize it.
        assert!(revised.changed);
        assert_ne!(revised.identity, validated.identity);
        assert_eq!(revised.patch.ops.len(), patch.ops.len());

        // Keeping everything again restores the original identity.
        view.selection.clear();
        let unchanged = view.revised(&patch, &contents);
        assert!(!unchanged.changed);
        assert_eq!(unchanged.identity, validated.identity);
    }

    #[test]
    fn the_revised_patch_contains_only_the_kept_change() {
        let mut fixture = Fixture::new("kept");
        let (patch, contents) = two_file_proposal(&mut fixture);
        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let changes = ChangeSet::from_patch(&validated, &contents);

        let mut view = ReviewView::new(changes);
        view.hunk_index = 1;
        view.reject_current_hunk().unwrap();

        let revised = view.revised(&patch, &contents);
        let crate::patch::PatchOp::Replace { content, .. } = &revised.patch.ops[0] else {
            panic!("expected a replace op");
        };
        // The kept change is present; the rejected one is not.
        assert!(content.contains("LINE 3 CHANGED"));
        assert!(!content.contains("LINE 36 CHANGED"));
        // And the rejected version is the one that would have been applied.
        let crate::patch::PatchOp::Replace {
            content: original_content,
            ..
        } = &patch.ops[0]
        else {
            panic!("expected a replace op");
        };
        assert!(original_content.contains("LINE 36 CHANGED"));
    }

    #[test]
    fn rejecting_every_hunk_of_a_creation_drops_the_file() {
        let fixture = Fixture::new("drop");
        fixture.write("src/a.rs", "one\n");
        let patch = Patch::new(
            "add a file",
            vec![PatchOp::Create {
                path: "src/new.rs".to_owned(),
                content: "brand new\n".to_owned(),
            }],
        );
        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let contents = BTreeMap::new();
        let changes = ChangeSet::from_patch(&validated, &contents);

        let mut view = ReviewView::new(changes);
        let hunk_ids = view.changes.hunk_ids();
        for id in &hunk_ids {
            view.selection.reject(id);
        }
        assert!(hunk_ids.iter().all(|id| view.selection.is_rejected(id)));
        let revised = view.revised(&patch, &contents);
        // Nothing is created: the proposal is empty, and its identity is
        // therefore different from the original.
        assert!(revised.patch.ops.is_empty());
        assert!(revised.changed);
    }

    #[test]
    fn an_approval_is_refused_when_the_proposal_is_stale() {
        let mut approvals = ApprovalState::new();
        let view = ApprovalView {
            action: "apply patch abc".to_owned(),
            workspace: "/ws".to_owned(),
            reason: "writes files".to_owned(),
            scope: vec!["src".to_owned()],
            network: false,
            approval_key: "fp-1".to_owned(),
            generation: 0,
        };
        let first = approvals.present(view.clone(), false);
        assert_eq!(first, 1);

        // A newer request supersedes it.
        let second = approvals.present(view, false);
        assert_eq!(second, 2);

        let err = approvals.answer(first, &BTreeMap::new(), &[]).unwrap_err();
        assert!(matches!(err, ApprovalError::Stale { .. }));

        // Answering the current one works.
        assert_eq!(
            approvals.answer(second, &BTreeMap::new(), &[]).unwrap(),
            ApprovalChoice::ApproveOnce
        );
        assert!(approvals.pending().is_none());
    }

    #[test]
    fn a_concurrent_edit_invalidates_an_approval() {
        let mut approvals = ApprovalState::new();
        let view = ApprovalView {
            action: "replace src/a.rs".to_owned(),
            workspace: "/ws".to_owned(),
            reason: "writes files".to_owned(),
            scope: vec!["src".to_owned()],
            network: false,
            approval_key: "fp".to_owned(),
            generation: 0,
        };
        let generation = approvals.present(view, false);

        let expected = crate::workspace::content_hash(b"original\n");
        let preconditions = vec![("src/a.rs".to_owned(), expected)];

        // The file changed under us.
        let mut current = BTreeMap::new();
        current.insert("src/a.rs".to_owned(), "edited by someone else\n".to_owned());
        let err = approvals
            .answer(generation, &current, &preconditions)
            .unwrap_err();
        assert!(matches!(err, ApprovalError::Conflicts { .. }));

        // With the matching content it goes through.
        let mut current = BTreeMap::new();
        current.insert("src/a.rs".to_owned(), "original\n".to_owned());
        assert!(
            approvals
                .answer(generation, &current, &preconditions)
                .is_ok()
        );
    }

    #[test]
    fn a_prompt_arriving_during_typing_cannot_capture_a_keystroke() {
        let mut approvals = ApprovalState::new();
        let view = ApprovalView {
            action: "run cargo test".to_owned(),
            workspace: "/ws".to_owned(),
            reason: "runs repository code".to_owned(),
            scope: vec![".".to_owned()],
            network: false,
            approval_key: "fp".to_owned(),
            generation: 0,
        };
        // The user was mid-prompt when the request arrived.
        approvals.present(view, true);
        assert!(approvals.arrived_during_typing());
        // Approve/deny shortcuts are therefore not armed: the next key
        // belongs to the composer.
        assert!(!approvals.shortcuts_armed(true));
        // Once the composer is empty the shortcuts become available.
        assert!(approvals.shortcuts_armed(false));
    }

    #[test]
    fn checks_are_labelled_honestly_and_never_green_when_stale() {
        let evidence = CheckEvidence {
            name: "test".to_owned(),
            description: "tests".to_owned(),
            outcome: CheckOutcome::Passed,
            revision: crate::ArtifactRevision::new("src", "rev-old"),
            command: vec!["cargo".to_owned(), "test".to_owned()],
            exit_code: Some(0),
            signal: None,
            duration_ms: 10,
            output: "test result: ok. 5 passed".to_owned(),
            output_truncated: false,
            test_counts: Default::default(),
            reason: "passed".to_owned(),
        };

        // Against the current revision it is green.
        let row = CheckRow::from_evidence(&evidence, "rev-old");
        assert!(row.is_green());
        assert_eq!(row.label(), "passed");

        // Against a newer revision the same evidence is stale, and the
        // label says so rather than showing a green pass.
        let row = CheckRow::from_evidence(&evidence, "rev-new");
        assert!(!row.is_green());
        assert!(row.label().contains("stale"));

        // A failed check is never green even at the right revision.
        let mut failed = evidence.clone();
        failed.outcome = CheckOutcome::Failed;
        let row = CheckRow::from_evidence(&failed, "rev-old");
        assert!(!row.is_green());
        assert_eq!(row.label(), "failed");
    }

    #[test]
    fn diagnostics_are_extracted_for_jumping() {
        let evidence = CheckEvidence {
            name: "build".to_owned(),
            description: "build".to_owned(),
            outcome: CheckOutcome::Failed,
            revision: crate::ArtifactRevision::new("src", "r"),
            command: vec!["cargo".to_owned(), "build".to_owned()],
            exit_code: Some(1),
            signal: None,
            duration_ms: 5,
            output: "compiling\\nerror[E0308]: mismatched types\\n  --> src/a.rs:3\\n\\nwarning: unused\\nfailed to build".to_owned(),
            output_truncated: false,
            test_counts: Default::default(),
            reason: "exited with code 1".to_owned(),
        };
        let row = CheckRow::from_evidence(&evidence, "r");
        assert!(row.diagnostics.iter().any(|d| d.contains("E0308")));
        assert!(row.diagnostics.len() <= 5);
    }

    #[test]
    fn keyboard_navigation_reaches_every_file_and_hunk() {
        let mut fixture = Fixture::new("nav");
        let (patch, contents) = two_file_proposal(&mut fixture);
        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let changes = ChangeSet::from_patch(&validated, &contents);
        let hunk_total = changes.hunk_ids().len();

        let mut view = ReviewView::new(changes);
        let mut visited = vec![view.current_hunk().unwrap().id.clone()];
        for _ in 1..hunk_total {
            view.next_hunk();
            visited.push(view.current_hunk().unwrap().id.clone());
        }
        let mut unique = visited.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), hunk_total, "navigation skipped a hunk");

        // And back to the start.
        for _ in 1..hunk_total {
            view.previous_hunk();
        }
        assert_eq!(view.file_index, 0);
        assert_eq!(view.hunk_index, 0);
    }

    #[test]
    fn the_summary_reports_changed_files_checks_and_limitations() {
        let mut fixture = Fixture::new("summary");
        let (patch, contents) = two_file_proposal(&mut fixture);
        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let changes = ChangeSet::from_patch(&validated, &contents);

        let mut view = ReviewView::new(changes);
        view.checks = vec![
            CheckRow {
                name: "build".to_owned(),
                state: CheckOutcome::Passed,
                revision: "r1".to_owned(),
                current_revision: true,
                summary: "passed".to_owned(),
                diagnostics: Vec::new(),
                output_truncated: false,
            },
            CheckRow {
                name: "test".to_owned(),
                state: CheckOutcome::Inconclusive,
                revision: "r1".to_owned(),
                current_revision: true,
                summary: "no tests were discovered".to_owned(),
                diagnostics: Vec::new(),
                output_truncated: false,
            },
        ];

        let summary = view.summary();
        assert!(summary.contains("file(s) changed"));
        assert!(summary.contains("src/a.rs"));
        assert!(summary.contains("checks actually run"));
        assert!(summary.contains("build: passed"));
        // The unresolved check is stated as a limitation, not hidden.
        assert!(summary.contains("limitations"));
        assert!(summary.contains("test: inconclusive"));
    }

    #[test]
    fn large_diffs_stay_bounded() {
        // A pathological diff falls back to a summary hunk rather than
        // attempting an enormous table.
        let before = (0..5_000)
            .map(|i| format!("old {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let after = (0..5_000)
            .map(|i| format!("new {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        let started = std::time::Instant::now();
        let hunks = diff_hunks("big.txt", &before, &after, 0);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!hunks.is_empty());
        // The fallback states what happened rather than rendering nothing.
        assert!(hunks[0].lines[0].contains("diff too large"));
    }

    #[test]
    fn unicode_and_long_lines_survive_diffing() {
        let before = "räksmörgås\\n日本語\\nemoji 👨‍👩‍👧\\n";
        let after = "räksmörgås ändrad\\n日本語\\nemoji 👨‍👩‍👧\\n";
        let hunks = diff_hunks("s.txt", before, after, 0);
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].lines.iter().any(|line| line.contains("ändrad")));
        assert!(
            hunks[0]
                .lines
                .iter()
                .any(|line| line.starts_with('-') && line.contains("räksmörgås"))
        );
    }

    #[test]
    fn a_keyboard_user_reviews_two_files_rejects_a_hunk_and_approves_the_revision() {
        // The acceptance test for #29, driven only through the view's
        // keyboard-level operations.
        let mut fixture = Fixture::new("keyboard");
        let (patch, contents) = two_file_proposal(&mut fixture);
        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let changes = ChangeSet::from_patch(&validated, &contents);
        let mut view = ReviewView::new(changes);

        // Move through both files.
        assert_eq!(view.current_file().unwrap().path, "src/a.rs");
        view.next_file();
        assert_eq!(view.current_file().unwrap().path, "src/b.rs");
        view.previous_file();
        assert_eq!(view.current_file().unwrap().path, "src/a.rs");

        // Reject the second hunk of the first file.
        view.next_hunk();
        let rejected = view.reject_current_hunk().unwrap();
        assert!(view.selection.is_rejected(&rejected));

        // The revised proposal is a new one, needing its own approval.
        let revised = view.revised(&patch, &contents);
        assert!(revised.changed);
        assert_ne!(revised.identity, validated.identity);

        // The revised patch still validates and applies against the
        // workspace, and the rejected change is gone.
        let revalidated = validate_patch(&workspace, &revised.patch).unwrap();
        let mut applier = crate::patch::PatchApplier::new();
        applier.apply(&workspace, &revalidated).unwrap();
        let applied = std::fs::read_to_string(fixture.dir.join("src/a.rs")).unwrap();
        assert!(applied.contains("LINE 3 CHANGED"));
        assert!(!applied.contains("LINE 36 CHANGED"));

        // Checks then run against that revision: the evidence names the
        // revision it saw, and the view refuses to show stale green.
        let revision = crate::ArtifactRevision::new(
            "src/a.rs",
            crate::workspace::content_hash(applied.as_bytes()),
        );
        let evidence = CheckEvidence {
            name: "test".to_owned(),
            description: "tests".to_owned(),
            outcome: CheckOutcome::Passed,
            revision: revision.clone(),
            command: vec!["cargo".to_owned(), "test".to_owned()],
            exit_code: Some(0),
            signal: None,
            duration_ms: 20,
            output: "test result: ok. 3 passed; 0 failed".to_owned(),
            output_truncated: false,
            test_counts: Default::default(),
            reason: "passed".to_owned(),
        };
        view.checks = vec![CheckRow::from_evidence(&evidence, &revision.revision)];
        assert!(view.checks[0].is_green());

        // A concurrent edit invalidates the displayed approval and makes
        // the previously green check stale immediately.
        let mut approvals = ApprovalState::new();
        let generation = approvals.present(
            ApprovalView {
                action: format!("apply {}", revised.identity),
                workspace: fixture.dir.to_string_lossy().into_owned(),
                reason: "writes files".to_owned(),
                scope: vec!["src".to_owned()],
                network: false,
                approval_key: "fp".to_owned(),
                generation: 0,
            },
            false,
        );
        fixture.write("src/a.rs", "someone else edited this\n");
        let current: BTreeMap<String, String> = contents
            .keys()
            .map(|path| {
                (
                    path.clone(),
                    std::fs::read_to_string(fixture.dir.join(path)).unwrap(),
                )
            })
            .collect();
        let err = approvals
            .answer(generation, &current, &revalidated.preconditions)
            .unwrap_err();
        assert!(matches!(err, ApprovalError::Conflicts { .. }));

        let stale_row = CheckRow::from_evidence(&evidence, "a-different-revision");
        assert!(!stale_row.is_green());
        assert!(stale_row.label().contains("stale"));
    }
}
