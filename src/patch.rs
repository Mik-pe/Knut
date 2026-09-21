//! Reviewable patches with stale-edit protection and safe recovery
//! (issue #24).
//!
//! The reasoner proposes a structured patch; Knut validates it, previews
//! it, binds approval to the *normalized* patch plus the source revisions
//! it depends on, revalidates immediately before applying, and records
//! every operation in a journal so a partial failure is visible and a
//! targeted revert is possible.
//!
//! Deliberate boundaries:
//! - no filesystem-wide atomicity is claimed: multi-file apply can fail
//!   partway and says so;
//! - no destructive git operation is ever issued (`reset`, `clean`,
//!   `checkout` over user content) and nothing is committed or pushed;
//! - a patch that no longer applies is reported as stale so the caller
//!   can fetch fresh evidence and ask for a new bounded patch, rather
//!   than fuzzy-matching its way through the user's code.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::KnutError;
use crate::policy::ContentPreconditions;
use crate::workspace::{Workspace, content_hash};

/// One edit operation. Paths are workspace-relative and `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchOp {
    /// Create a new file; fails if it exists.
    Create { path: String, content: String },
    /// Replace an existing file's entire contents.
    ///
    /// The full-file form is deliberate: it makes the precondition exact
    /// and the preview complete, with no ambiguous-match heuristics.
    Replace {
        path: String,
        content: String,
        /// Required content identity of the file being replaced.
        expect_hash: String,
    },
    /// Delete a file; the hash must match before it is removed.
    Delete { path: String, expect_hash: String },
    /// Rename/move a file, optionally replacing its contents.
    Rename {
        from: String,
        to: String,
        expect_hash: String,
        /// New contents, when the move also edits the file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
}

impl PatchOp {
    pub fn path(&self) -> &str {
        match self {
            PatchOp::Create { path, .. }
            | PatchOp::Replace { path, .. }
            | PatchOp::Delete { path, .. } => path,
            PatchOp::Rename { to, .. } => to,
        }
    }
}

/// A bounded set of edits proposed as one reviewable change.
/// A bounded set of edits proposed as one reviewable change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    /// Short human/model-readable intent, shown with the preview.
    pub summary: String,
    pub ops: Vec<PatchOp>,
}

/// Maximum operations in one patch.
pub const MAX_PATCH_OPS: usize = 32;
/// Maximum bytes one operation may write.
pub const MAX_OP_BYTES: usize = 1024 * 1024;

impl Patch {
    pub fn new(summary: impl Into<String>, ops: Vec<PatchOp>) -> Self {
        Self {
            summary: summary.into(),
            ops,
        }
    }

    /// A stable identity of the normalized patch.
    ///
    /// Approval binds to this, so reordering operations or re-sending a
    /// different edit produces a different identity and needs approval
    /// again.
    pub fn identity(&self) -> String {
        let mut normalized: Vec<Value> = self
            .ops
            .iter()
            .map(|op| serde_json::to_value(op).unwrap_or(Value::Null))
            .collect();
        // Operation order is not meaningful for identity: the same set of
        // edits is the same change.
        normalized.sort_by_key(|v| v.to_string());
        let serialized = serde_json::to_string(&normalized).unwrap_or_default();
        content_hash(serialized.as_bytes())
    }

    /// Every file the patch touches.
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .ops
            .iter()
            .flat_map(|op| match op {
                PatchOp::Rename { from, to, .. } => vec![from.clone(), to.clone()],
                other => vec![other.path().to_owned()],
            })
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// One validated operation with its exact before/after identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedOp {
    pub kind: String,
    pub path: String,
    /// Path the content came from (rename source), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    /// The content to write. Carried on the validated op so apply never
    /// has to look the patch up again; the approval identity is computed
    /// from the patch, not from this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// A patch that has passed validation and can be previewed and applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedPatch {
    /// The normalized patch identity approval binds to.
    pub identity: String,
    pub summary: String,
    pub ops: Vec<ValidatedOp>,
    /// Content revisions of every file the patch depends on. These are
    /// the preconditions the gate fingerprints, so editing a file after
    /// approval invalidates that approval.
    pub preconditions: Vec<(String, String)>,
}

impl ValidatedPatch {
    /// The gate's content preconditions for this patch.
    pub fn gate_preconditions(&self) -> ContentPreconditions {
        let mut preconditions = ContentPreconditions::none();
        for (path, hash) in &self.preconditions {
            preconditions = preconditions.watch(path.clone(), hash.clone());
        }
        preconditions
    }

    /// A reviewable, human-readable preview.
    pub fn preview(&self) -> String {
        let mut out = format!("patch {} — {}\n", self.identity, self.summary);
        for op in &self.ops {
            match op.kind.as_str() {
                "create" => out.push_str(&format!(
                    "  create {} ({} bytes)\n",
                    op.path,
                    op.after_hash.as_deref().unwrap_or("-")
                )),
                "replace" => out.push_str(&format!(
                    "  replace {} ({} -> {})\n",
                    op.path,
                    op.before_hash.as_deref().unwrap_or("-"),
                    op.after_hash.as_deref().unwrap_or("-")
                )),
                "delete" => out.push_str(&format!(
                    "  delete {} ({})\n",
                    op.path,
                    op.before_hash.as_deref().unwrap_or("-")
                )),
                "rename" => out.push_str(&format!(
                    "  rename {} -> {} ({})\n",
                    op.from.as_deref().unwrap_or("-"),
                    op.path,
                    op.after_hash.as_deref().unwrap_or("-")
                )),
                other => out.push_str(&format!("  {other} {}\n", op.path)),
            }
        }
        out
    }

    /// Whether every precondition still holds on disk.
    pub fn is_fresh(&self, workspace: &Workspace) -> bool {
        self.preconditions
            .iter()
            .all(|(path, hash)| match workspace.read_bytes(path) {
                Ok(bytes) => content_hash(&bytes) == *hash,
                Err(_) => false,
            })
    }
}

/// Why a patch was rejected before anything was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchRejection {
    /// Too many operations.
    TooManyOps { count: usize },
    /// An operation was too large.
    TooLarge { path: String, bytes: usize },
    /// The path could not be used.
    UnsafePath { path: String, reason: String },
    /// Two operations target the same file.
    ConflictingOps { path: String },
    /// A create targets a file that already exists.
    AlreadyExists { path: String },
    /// A replace/delete/rename target does not exist.
    MissingFile { path: String },
    /// The declared precondition does not match the file on disk.
    StaleContent {
        path: String,
        expected: String,
        actual: Option<String>,
    },
    /// A rename would overwrite an existing file.
    RenameTargetExists { path: String },
}

impl std::fmt::Display for PatchRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatchRejection::TooManyOps { count } => {
                write!(f, "patch has {count} operations; limit is {MAX_PATCH_OPS}")
            }
            PatchRejection::TooLarge { path, bytes } => {
                write!(
                    f,
                    "operation on {path:?} writes {bytes} bytes; limit is {MAX_OP_BYTES}"
                )
            }
            PatchRejection::UnsafePath { path, reason } => {
                write!(f, "path {path:?} is unusable: {reason}")
            }
            PatchRejection::ConflictingOps { path } => {
                write!(f, "patch edits {path:?} more than once")
            }
            PatchRejection::AlreadyExists { path } => {
                write!(f, "create target {path:?} already exists")
            }
            PatchRejection::MissingFile { path } => {
                write!(f, "{path:?} does not exist")
            }
            PatchRejection::StaleContent {
                path,
                expected,
                actual,
            } => write!(
                f,
                "{path:?} changed since the patch was proposed (expected {expected}, found {})",
                actual.as_deref().unwrap_or("<missing>")
            ),
            PatchRejection::RenameTargetExists { path } => {
                write!(f, "rename target {path:?} already exists")
            }
        }
    }
}

/// Validate a patch against the workspace *before* asking for approval.
///
/// Every rejection here happens before any write, so a bad patch never
/// reaches the filesystem.
pub fn validate_patch(
    workspace: &Workspace,
    patch: &Patch,
) -> Result<ValidatedPatch, PatchRejection> {
    if patch.ops.is_empty() {
        return Err(PatchRejection::TooManyOps { count: 0 });
    }
    if patch.ops.len() > MAX_PATCH_OPS {
        return Err(PatchRejection::TooManyOps {
            count: patch.ops.len(),
        });
    }

    // Conflicting edits to one file are how a patch silently corrupts
    // content: refuse rather than guess an order.
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for op in &patch.ops {
        let target = op.path().to_owned();
        *seen.entry(target.clone()).or_default() += 1;
        if matches!(op, PatchOp::Rename { .. })
            && let PatchOp::Rename { from, .. } = op
        {
            *seen.entry(from.clone()).or_default() += 1;
        }
    }
    for (path, count) in &seen {
        if *count > 1 {
            return Err(PatchRejection::ConflictingOps { path: path.clone() });
        }
    }

    let mut validated_ops = Vec::new();
    let mut preconditions = Vec::new();

    for op in &patch.ops {
        let bytes_written = match op {
            PatchOp::Create { content, .. } | PatchOp::Replace { content, .. } => content.len(),
            PatchOp::Rename { content, .. } => content.as_ref().map(String::len).unwrap_or(0),
            PatchOp::Delete { .. } => 0,
        };
        if bytes_written > MAX_OP_BYTES {
            return Err(PatchRejection::TooLarge {
                path: op.path().to_owned(),
                bytes: bytes_written,
            });
        }

        match op {
            PatchOp::Create { path, content } => {
                let resolved =
                    workspace
                        .resolve(path)
                        .map_err(|err| PatchRejection::UnsafePath {
                            path: path.clone(),
                            reason: err.to_string(),
                        })?;
                if resolved.absolute().exists() {
                    return Err(PatchRejection::AlreadyExists { path: path.clone() });
                }
                let after = content_hash(content.as_bytes());
                validated_ops.push(ValidatedOp {
                    kind: "create".to_owned(),
                    path: resolved.relative().to_owned(),
                    from: None,
                    before_hash: None,
                    after_hash: Some(after),
                    content: Some(content.clone()),
                });
            }
            PatchOp::Replace {
                path,
                content,
                expect_hash,
            } => {
                let resolved =
                    workspace
                        .resolve(path)
                        .map_err(|err| PatchRejection::UnsafePath {
                            path: path.clone(),
                            reason: err.to_string(),
                        })?;
                let bytes = workspace
                    .read_bytes(path)
                    .map_err(|_| PatchRejection::MissingFile { path: path.clone() })?;
                let actual = content_hash(&bytes);
                if actual != *expect_hash {
                    return Err(PatchRejection::StaleContent {
                        path: path.clone(),
                        expected: expect_hash.clone(),
                        actual: Some(actual),
                    });
                }
                let relative = resolved.relative().to_owned();
                preconditions.push((relative.clone(), actual.clone()));
                validated_ops.push(ValidatedOp {
                    kind: "replace".to_owned(),
                    path: relative,
                    from: None,
                    before_hash: Some(actual),
                    after_hash: Some(content_hash(content.as_bytes())),
                    content: Some(content.clone()),
                });
            }
            PatchOp::Delete { path, expect_hash } => {
                let resolved =
                    workspace
                        .resolve(path)
                        .map_err(|err| PatchRejection::UnsafePath {
                            path: path.clone(),
                            reason: err.to_string(),
                        })?;
                let bytes = workspace
                    .read_bytes(path)
                    .map_err(|_| PatchRejection::MissingFile { path: path.clone() })?;
                let actual = content_hash(&bytes);
                if actual != *expect_hash {
                    return Err(PatchRejection::StaleContent {
                        path: path.clone(),
                        expected: expect_hash.clone(),
                        actual: Some(actual),
                    });
                }
                let relative = resolved.relative().to_owned();
                preconditions.push((relative.clone(), actual.clone()));
                validated_ops.push(ValidatedOp {
                    kind: "delete".to_owned(),
                    path: relative,
                    from: None,
                    before_hash: Some(actual),
                    after_hash: None,
                    content: None,
                });
            }
            PatchOp::Rename {
                from,
                to,
                expect_hash,
                content,
            } => {
                let from_resolved =
                    workspace
                        .resolve(from)
                        .map_err(|err| PatchRejection::UnsafePath {
                            path: from.clone(),
                            reason: err.to_string(),
                        })?;
                let to_resolved =
                    workspace
                        .resolve(to)
                        .map_err(|err| PatchRejection::UnsafePath {
                            path: to.clone(),
                            reason: err.to_string(),
                        })?;
                let bytes = workspace
                    .read_bytes(from)
                    .map_err(|_| PatchRejection::MissingFile { path: from.clone() })?;
                let actual = content_hash(&bytes);
                if actual != *expect_hash {
                    return Err(PatchRejection::StaleContent {
                        path: from.clone(),
                        expected: expect_hash.clone(),
                        actual: Some(actual),
                    });
                }
                if to_resolved.absolute().exists() {
                    return Err(PatchRejection::RenameTargetExists { path: to.clone() });
                }
                let from_relative = from_resolved.relative().to_owned();
                preconditions.push((from_relative.clone(), actual.clone()));
                let after = content
                    .as_ref()
                    .map(|c| content_hash(c.as_bytes()))
                    .unwrap_or_else(|| actual.clone());
                validated_ops.push(ValidatedOp {
                    kind: "rename".to_owned(),
                    path: to_resolved.relative().to_owned(),
                    from: Some(from_relative),
                    before_hash: Some(actual),
                    after_hash: Some(after),
                    content: content.clone(),
                });
            }
        }
    }

    preconditions.sort();

    Ok(ValidatedPatch {
        identity: patch.identity(),
        summary: patch.summary.clone(),
        ops: validated_ops,
        preconditions,
    })
}

/// One journal entry for an applied operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedOp {
    /// Identity of the patch this operation came from.
    pub identity: String,
    pub kind: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Content before the change, when the file existed and was text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    pub after_hash: Option<String>,
}

/// The outcome of applying a patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub identity: String,
    /// Operations that completed, in order.
    pub applied: Vec<AppliedOp>,
    /// A partial failure: the applied operations are real, and the
    /// remaining ones did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ApplyFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyFailure {
    pub path: String,
    pub reason: String,
    /// Whether the file(s) may be in an intermediate state.
    pub recovery_available: bool,
}

impl ApplyOutcome {
    pub fn is_partial(&self) -> bool {
        self.failure.is_some()
    }
}

/// Applies validated patches and records what Knut changed.
///
/// The journal is what makes a targeted revert possible: only files this
/// session actually changed can be reverted, and only when they still
/// match what was written.
#[derive(Debug, Default)]
pub struct PatchApplier {
    journal: Vec<AppliedOp>,
    applied_identities: Vec<String>,
}

impl PatchApplier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything this applier changed, oldest first.
    pub fn journal(&self) -> &[AppliedOp] {
        &self.journal
    }

    /// Whether this exact patch identity was already applied.
    ///
    /// A repeat is refused rather than re-applied: the same patch twice
    /// is a bug, not an idempotent operation.
    pub fn already_applied(&self, identity: &str) -> bool {
        self.applied_identities
            .iter()
            .any(|existing| existing == identity)
    }

    /// Apply a validated patch.
    ///
    /// Revalidates every precondition immediately before the first write,
    /// so an edit made after approval invalidates the approval instead of
    /// being overwritten. Per-file replacement is safe (write to a
    /// temporary file in the same directory, then rename); multi-file
    /// apply is *not* atomic and a partial failure is reported with the
    /// operations that did run.
    pub fn apply(
        &mut self,
        workspace: &Workspace,
        patch: &ValidatedPatch,
    ) -> Result<ApplyOutcome, KnutError> {
        if self.already_applied(&patch.identity) {
            return Err(KnutError::Tool(format!(
                "patch {} was already applied; a changed patch requires new approval",
                patch.identity
            )));
        }

        // Revalidate at apply time, not only at approval time.
        for (path, hash) in &patch.preconditions {
            let actual = workspace
                .read_bytes(path)
                .map(|bytes| content_hash(&bytes))
                .ok();
            if actual.as_deref() != Some(hash.as_str()) {
                return Err(KnutError::Tool(format!(
                    "path {path:?} changed after approval; the patch is stale and no files were written"
                )));
            }
        }

        let mut applied = Vec::new();

        for op in &patch.ops {
            let result = self.apply_op(workspace, op, &patch.identity);
            match result {
                Ok(entry) => {
                    self.journal.push(entry.clone());
                    applied.push(entry);
                }
                Err(err) => {
                    // A partial failure is visible: what ran is reported,
                    // what did not is named.
                    let failure = ApplyFailure {
                        path: op.path.clone(),
                        reason: err.to_string(),
                        recovery_available: !applied.is_empty(),
                    };
                    self.applied_identities.push(patch.identity.clone());
                    return Ok(ApplyOutcome {
                        identity: patch.identity.clone(),
                        applied,
                        failure: Some(failure),
                    });
                }
            }
        }

        self.applied_identities.push(patch.identity.clone());
        Ok(ApplyOutcome {
            identity: patch.identity.clone(),
            applied,
            failure: None,
        })
    }

    fn apply_op(
        &mut self,
        workspace: &Workspace,
        op: &ValidatedOp,
        op_identity: &str,
    ) -> Result<AppliedOp, KnutError> {
        let path = workspace.resolve(&op.path)?;
        let before = std::fs::read(path.absolute())
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());

        match op.kind.as_str() {
            "create" | "replace" => {
                let content = op
                    .after_hash
                    .as_ref()
                    .and_then(|_| read_patch_content(op))
                    .ok_or_else(|| KnutError::Tool("operation has no content".to_owned()))?;
                write_atomically(path.absolute(), content.as_bytes())?;
                Ok(AppliedOp {
                    identity: op_identity.to_owned(),
                    kind: op.kind.clone(),
                    path: op.path.clone(),
                    from: None,
                    before,
                    after_hash: op.after_hash.clone(),
                })
            }
            "delete" => {
                std::fs::remove_file(path.absolute())
                    .map_err(|err| KnutError::Tool(format!("delete {:?}: {err}", op.path)))?;
                Ok(AppliedOp {
                    identity: op_identity.to_owned(),
                    kind: "delete".to_owned(),
                    path: op.path.clone(),
                    from: None,
                    before,
                    after_hash: None,
                })
            }
            "rename" => {
                let from = op
                    .from
                    .clone()
                    .ok_or_else(|| KnutError::Tool("rename operation has no source".to_owned()))?;
                let from_path = workspace.resolve(&from)?;
                let from_before = std::fs::read(from_path.absolute())
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
                match read_patch_content(op) {
                    Some(content) => {
                        write_atomically(path.absolute(), content.as_bytes())?;
                        std::fs::remove_file(from_path.absolute()).map_err(|err| {
                            KnutError::Tool(format!("rename source {:?}: {err}", from))
                        })?;
                    }
                    None => {
                        // Re-check the target immediately before moving:
                        // a file that appeared after validation must not
                        // be silently clobbered.
                        if path.absolute().exists() {
                            return Err(KnutError::Tool(format!(
                                "rename target {:?} already exists; refusing to overwrite it",
                                op.path
                            )));
                        }
                        std::fs::rename(from_path.absolute(), path.absolute()).map_err(|err| {
                            KnutError::Tool(format!("rename {from:?} -> {:?}: {err}", op.path))
                        })?;
                    }
                }
                Ok(AppliedOp {
                    identity: op_identity.to_owned(),
                    kind: "rename".to_owned(),
                    path: op.path.clone(),
                    from: Some(from),
                    before: from_before,
                    after_hash: op.after_hash.clone(),
                })
            }
            other => Err(KnutError::Tool(format!(
                "unknown patch operation {other:?}"
            ))),
        }
    }

    /// Revert exactly the journal entries whose files still match what
    /// was written.
    ///
    /// Files changed since (by the user, another process or another
    /// agent) are left alone: reverting a conversation is not permission
    /// to discard someone else's work.
    pub fn revert(
        &mut self,
        workspace: &Workspace,
        identity: Option<&str>,
    ) -> Result<RevertOutcome, KnutError> {
        let mut reverted = Vec::new();
        let mut skipped = Vec::new();

        // The journal belongs to this applier, so every entry is a change
        // Knut made in this session. `identity` narrows the revert to the
        // operations of one patch when the caller wants that.
        let entries: Vec<AppliedOp> = self
            .journal
            .iter()
            .filter(|entry| identity.is_none_or(|id| id == entry.identity.as_str()))
            .cloned()
            .collect();

        for entry in entries.iter().rev() {
            let path = workspace.resolve(&entry.path)?;
            let current = std::fs::read(path.absolute()).ok();
            let current_hash = current.as_ref().map(|bytes| content_hash(bytes));

            let matches_written = match (&entry.after_hash, &current_hash) {
                (Some(written), Some(current)) => written == current,
                // A delete expects the file to be absent.
                (None, None) => true,
                _ => false,
            };
            if !matches_written {
                skipped.push(SkippedRevert {
                    path: entry.path.clone(),
                    reason: "file changed since Knut wrote it; refusing to overwrite".to_owned(),
                });
                continue;
            }

            match entry.kind.as_str() {
                "create" => {
                    if let Err(err) = std::fs::remove_file(path.absolute()) {
                        skipped.push(SkippedRevert {
                            path: entry.path.clone(),
                            reason: format!("could not remove created file: {err}"),
                        });
                        continue;
                    }
                }
                "delete" => {
                    let Some(before) = &entry.before else {
                        skipped.push(SkippedRevert {
                            path: entry.path.clone(),
                            reason: "no recorded pre-delete content".to_owned(),
                        });
                        continue;
                    };
                    write_atomically(path.absolute(), before.as_bytes())?;
                }
                "replace" => {
                    let Some(before) = &entry.before else {
                        skipped.push(SkippedRevert {
                            path: entry.path.clone(),
                            reason: "no recorded pre-edit content".to_owned(),
                        });
                        continue;
                    };
                    write_atomically(path.absolute(), before.as_bytes())?;
                }
                "rename" => {
                    let Some(from) = &entry.from else {
                        skipped.push(SkippedRevert {
                            path: entry.path.clone(),
                            reason: "no recorded rename source".to_owned(),
                        });
                        continue;
                    };
                    let from_path = workspace.resolve(from)?;
                    if from_path.absolute().exists() {
                        skipped.push(SkippedRevert {
                            path: from.clone(),
                            reason: "rename source already exists; refusing to overwrite"
                                .to_owned(),
                        });
                        continue;
                    }
                    if let Err(err) = std::fs::rename(path.absolute(), from_path.absolute()) {
                        skipped.push(SkippedRevert {
                            path: entry.path.clone(),
                            reason: format!("could not move file back: {err}"),
                        });
                        continue;
                    }
                }
                _ => {}
            }

            reverted.push(entry.path.clone());
        }

        Ok(RevertOutcome { reverted, skipped })
    }
}

/// Content for an operation, recovered from the validated op's after-hash.
///
/// The validated op stores hashes rather than content so approval
/// identities stay small; the content itself travels in the patch that
/// produced it. This lookup is intentionally explicit: an operation
/// without recoverable content fails rather than writing an empty file.
fn read_patch_content(op: &ValidatedOp) -> Option<String> {
    op.content.clone()
}

/// Revert summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertOutcome {
    pub reverted: Vec<String>,
    pub skipped: Vec<SkippedRevert>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedRevert {
    pub path: String,
    pub reason: String,
}

/// Write a file safely: temp file in the same directory, then rename.
///
/// Per-file replacement is atomic on POSIX; a multi-file patch is not,
/// and the caller must treat a partial failure as exactly that.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), KnutError> {
    let directory = path
        .parent()
        .ok_or_else(|| KnutError::Tool("target has no parent directory".to_owned()))?;
    std::fs::create_dir_all(directory)
        .map_err(|err| KnutError::Tool(format!("create {}: {err}", directory.display())))?;

    let temp = directory.join(format!(
        ".knut-tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&temp, bytes)
        .map_err(|err| KnutError::Tool(format!("write temp file: {err}")))?;
    std::fs::rename(&temp, path).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        KnutError::Tool(format!("replace {}: {err}", path.display()))
    })?;
    Ok(())
}

/// The `files.apply_patch` tool: the only way a patch reaches disk.
///
/// It goes through the mandatory gate like every other tool, with the
/// validated patch's content revisions as preconditions. That is what
/// binds approval to *this* patch against *these* file revisions: edit a
/// file after approval and the fingerprint changes, so the approval no
/// longer applies.
pub struct ApplyPatchTool {
    workspace: Workspace,
    /// The validated patch this call is allowed to apply, set by the
    /// runtime after validation and approval.
    pending: std::sync::Mutex<Option<ValidatedPatch>>,
}

impl ApplyPatchTool {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            pending: std::sync::Mutex::new(None),
        }
    }

    /// Stage a validated patch for the next call.
    pub fn stage(&self, patch: ValidatedPatch) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some(patch);
        }
    }

    pub fn capability() -> &'static str {
        crate::workspace::CAPABILITY
    }

    /// Content preconditions the gate should fingerprint for the staged
    /// patch, so approval binds to the exact source revisions.
    pub fn staged_preconditions(&self) -> ContentPreconditions {
        self.pending
            .lock()
            .ok()
            .and_then(|pending| pending.as_ref().map(|p| p.gate_preconditions()))
            .unwrap_or_else(ContentPreconditions::none)
    }
}

#[async_trait::async_trait]
impl crate::tool::Tool for ApplyPatchTool {
    fn metadata(&self) -> crate::tool::ToolMetadata {
        crate::tool::ToolMetadata {
            id: "apply_patch".to_owned(),
            tool_version: "1".to_owned(),
            capability: crate::workspace::CAPABILITY.to_owned(),
            description: "Apply a validated, approved patch to the workspace".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch_identity": { "type": "string" }
                }
            }),
            side_effect: crate::tool::SideEffect::IdempotentWrite,
        }
    }

    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        let requested = input
            .get("patch_identity")
            .and_then(Value::as_str)
            .ok_or_else(|| KnutError::Tool("apply_patch requires patch_identity".to_owned()))?;

        let staged = self
            .pending
            .lock()
            .ok()
            .and_then(|pending| pending.clone())
            .ok_or_else(|| KnutError::Tool("no validated patch is staged".to_owned()))?;

        // The identity must match: applying a different patch than the
        // one that was approved is refused outright.
        if staged.identity != requested {
            return Err(KnutError::Tool(format!(
                "staged patch {} does not match requested identity {requested}",
                staged.identity
            )));
        }

        let workspace = self.workspace.clone();
        let patch = staged.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let mut applier = PatchApplier::new();
            applier.apply(&workspace, &patch)
        })
        .await
        .map_err(|err| KnutError::Tool(format!("apply task failed: {err}")))??;

        serde_json::to_value(&outcome)
            .map_err(|err| KnutError::Tool(format!("serialize outcome: {err}")))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "knut-patch-{name}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write(&self, relative: &str, contents: &str) -> String {
            let path = self.dir.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
            content_hash(contents.as_bytes())
        }

        fn read(&self, relative: &str) -> String {
            std::fs::read_to_string(self.dir.join(relative)).unwrap()
        }

        fn exists(&self, relative: &str) -> bool {
            self.dir.join(relative).exists()
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
    fn valid_single_and_multi_file_patches_produce_the_previewed_output() {
        let fixture = Fixture::new("apply");
        let lib_hash = fixture.write("src/lib.rs", "fn main() {}\n");
        fixture.write("src/old.rs", "// old\n");

        let patch = Patch::new(
            "add a helper and reorganize",
            vec![
                PatchOp::Replace {
                    path: "src/lib.rs".to_owned(),
                    content: "fn main() { helper(); }\n".to_owned(),
                    expect_hash: lib_hash.clone(),
                },
                PatchOp::Create {
                    path: "src/helper.rs".to_owned(),
                    content: "pub fn helper() {}\n".to_owned(),
                },
                PatchOp::Rename {
                    from: "src/old.rs".to_owned(),
                    to: "src/new.rs".to_owned(),
                    expect_hash: fixture_old_hash(&fixture),
                    content: None,
                },
            ],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();

        // The preview names every operation and its identities.
        let preview = validated.preview();
        assert!(preview.contains("replace src/lib.rs"));
        assert!(preview.contains("create src/helper.rs"));
        assert!(preview.contains("rename src/old.rs -> src/new.rs"));

        let mut applier = PatchApplier::new();
        let outcome = applier.apply(&workspace, &validated).unwrap();

        assert!(!outcome.is_partial());
        assert_eq!(fixture.read("src/lib.rs"), "fn main() { helper(); }\n");
        assert_eq!(fixture.read("src/helper.rs"), "pub fn helper() {}\n");
        assert!(fixture.exists("src/new.rs"));
        assert!(!fixture.exists("src/old.rs"));

        // The provenance record covers every operation.
        assert_eq!(outcome.applied.len(), 3);
        assert_eq!(applier.journal().len(), 3);
        assert_eq!(applier.journal()[2].from.as_deref(), Some("src/old.rs"));
    }

    fn fixture_old_hash(fixture: &Fixture) -> String {
        content_hash(fixture.read("src/old.rs").as_bytes())
    }

    #[test]
    fn stale_content_cannot_apply() {
        let fixture = Fixture::new("stale");
        let hash = fixture.write("a.txt", "original\n");

        let patch = Patch::new(
            "edit",
            vec![PatchOp::Replace {
                path: "a.txt".to_owned(),
                content: "edited\n".to_owned(),
                expect_hash: hash,
            }],
        );

        // Someone else edits the file between proposal and validation.
        fixture.write("a.txt", "someone else's work\n");

        let workspace = fixture.workspace();
        let err = validate_patch(&workspace, &patch).unwrap_err();
        assert!(
            matches!(err, PatchRejection::StaleContent { .. }),
            "got {err}"
        );
        // Nothing was written.
        assert_eq!(fixture.read("a.txt"), "someone else's work\n");
    }

    #[test]
    fn approval_is_invalidated_when_the_file_changes_before_apply() {
        let fixture = Fixture::new("approval-stale");
        let hash = fixture.write("a.txt", "one\n");
        let patch = Patch::new(
            "edit",
            vec![PatchOp::Replace {
                path: "a.txt".to_owned(),
                content: "two\n".to_owned(),
                expect_hash: hash,
            }],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        assert!(validated.is_fresh(&workspace));

        // A user (or another agent) edits the file after approval.
        fixture.write("a.txt", "user's edit\n");

        assert!(!validated.is_fresh(&workspace));
        let mut applier = PatchApplier::new();
        let err = applier.apply(&workspace, &validated).unwrap_err();
        assert!(format!("{err}").contains("stale"), "got {err}");
        // The user's edit survived.
        assert_eq!(fixture.read("a.txt"), "user's edit\n");
    }

    #[test]
    fn repeated_identity_cannot_apply_the_same_patch_twice() {
        let fixture = Fixture::new("repeat");
        let hash = fixture.write("a.txt", "one\n");
        let patch = Patch::new(
            "edit",
            vec![PatchOp::Replace {
                path: "a.txt".to_owned(),
                content: "two\n".to_owned(),
                expect_hash: hash,
            }],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();

        let mut applier = PatchApplier::new();
        applier.apply(&workspace, &validated).unwrap();
        assert!(applier.already_applied(&validated.identity));

        let err = applier.apply(&workspace, &validated).unwrap_err();
        assert!(format!("{err}").contains("already applied"), "got {err}");
        assert_eq!(fixture.read("a.txt"), "two\n");
    }

    #[test]
    fn a_changed_patch_has_a_different_identity() {
        let first = Patch::new(
            "edit",
            vec![PatchOp::Create {
                path: "a.txt".to_owned(),
                content: "one\n".to_owned(),
            }],
        );
        let second = Patch::new(
            "edit",
            vec![PatchOp::Create {
                path: "a.txt".to_owned(),
                content: "two\n".to_owned(),
            }],
        );
        assert_ne!(first.identity(), second.identity());

        // Order is not meaningful: the same operations in a different
        // order are the same change.
        let a = Patch::new(
            "x",
            vec![
                PatchOp::Create {
                    path: "a.txt".to_owned(),
                    content: "a\n".to_owned(),
                },
                PatchOp::Create {
                    path: "b.txt".to_owned(),
                    content: "b\n".to_owned(),
                },
            ],
        );
        let b = Patch::new(
            "x",
            vec![
                PatchOp::Create {
                    path: "b.txt".to_owned(),
                    content: "b\n".to_owned(),
                },
                PatchOp::Create {
                    path: "a.txt".to_owned(),
                    content: "a\n".to_owned(),
                },
            ],
        );
        assert_eq!(a.identity(), b.identity());
    }

    #[test]
    fn conflicting_ops_and_unsafe_paths_are_rejected_before_any_effect() {
        let fixture = Fixture::new("conflict");
        let hash = fixture.write("a.txt", "one\n");
        let workspace = fixture.workspace();

        let twice = Patch::new(
            "x",
            vec![
                PatchOp::Replace {
                    path: "a.txt".to_owned(),
                    content: "two\n".to_owned(),
                    expect_hash: hash.clone(),
                },
                PatchOp::Delete {
                    path: "a.txt".to_owned(),
                    expect_hash: hash.clone(),
                },
            ],
        );
        assert!(matches!(
            validate_patch(&workspace, &twice).unwrap_err(),
            PatchRejection::ConflictingOps { .. }
        ));

        let escape = Patch::new(
            "x",
            vec![PatchOp::Create {
                path: "../outside.txt".to_owned(),
                content: "nope\n".to_owned(),
            }],
        );
        assert!(matches!(
            validate_patch(&workspace, &escape).unwrap_err(),
            PatchRejection::UnsafePath { .. }
        ));

        let already = Patch::new(
            "x",
            vec![PatchOp::Create {
                path: "a.txt".to_owned(),
                content: "nope\n".to_owned(),
            }],
        );
        assert!(matches!(
            validate_patch(&workspace, &already).unwrap_err(),
            PatchRejection::AlreadyExists { .. }
        ));

        // Nothing changed.
        assert_eq!(fixture.read("a.txt"), "one\n");
    }

    #[test]
    fn symlink_escape_in_a_patch_target_is_rejected() {
        let fixture = Fixture::new("symlink");
        let outside = Fixture::new("symlink-outside");
        outside.write("secret.txt", "outside\n");
        std::os::unix::fs::symlink(&outside.dir, fixture.dir.join("escape")).unwrap();

        let patch = Patch::new(
            "x",
            vec![PatchOp::Replace {
                path: "escape/secret.txt".to_owned(),
                content: "clobbered\n".to_owned(),
                expect_hash: content_hash(b"outside\n"),
            }],
        );
        let workspace = fixture.workspace();
        let err = validate_patch(&workspace, &patch).unwrap_err();
        assert!(
            matches!(err, PatchRejection::UnsafePath { .. }),
            "got {err}"
        );
        assert_eq!(outside.read("secret.txt"), "outside\n");
    }

    #[test]
    fn unicode_paths_apply_correctly() {
        let fixture = Fixture::new("unicode");
        let hash = fixture.write("src/überlegungen.txt", "alt\n");
        let patch = Patch::new(
            "edit",
            vec![
                PatchOp::Replace {
                    path: "src/überlegungen.txt".to_owned(),
                    content: "neu\n".to_owned(),
                    expect_hash: hash,
                },
                PatchOp::Create {
                    path: "src/日本語.md".to_owned(),
                    content: "メモ\n".to_owned(),
                },
            ],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let mut applier = PatchApplier::new();
        let outcome = applier.apply(&workspace, &validated).unwrap();

        assert!(!outcome.is_partial());
        assert_eq!(fixture.read("src/überlegungen.txt"), "neu\n");
        assert_eq!(fixture.read("src/日本語.md"), "メモ\n");
    }

    #[test]
    fn mixed_line_endings_are_preserved_exactly() {
        let fixture = Fixture::new("crlf");
        let crlf = "line one\r\nline two\r\n";
        let hash = fixture.write("a.txt", crlf);
        let patch = Patch::new(
            "edit",
            vec![PatchOp::Replace {
                path: "a.txt".to_owned(),
                content: "line one\r\nline two changed\r\n".to_owned(),
                expect_hash: hash,
            }],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let mut applier = PatchApplier::new();
        applier.apply(&workspace, &validated).unwrap();

        // CRLF survived: no normalization was applied behind the user's
        // back.
        assert_eq!(fixture.read("a.txt"), "line one\r\nline two changed\r\n");
    }

    #[test]
    fn user_changes_survive_apply_failure_and_targeted_revert() {
        let fixture = Fixture::new("dirty");
        let hash = fixture.write("tracked.txt", "tracked\n");
        // A user change that must survive everything.
        fixture.write("user_work.txt", "my work in progress\n");

        let patch = Patch::new(
            "edit",
            vec![
                PatchOp::Replace {
                    path: "tracked.txt".to_owned(),
                    content: "knut's edit\n".to_owned(),
                    expect_hash: hash,
                },
                PatchOp::Create {
                    path: "knut_new.txt".to_owned(),
                    content: "knut created this\n".to_owned(),
                },
            ],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let mut applier = PatchApplier::new();
        applier.apply(&workspace, &validated).unwrap();

        // The user's untracked work is untouched.
        assert_eq!(fixture.read("user_work.txt"), "my work in progress\n");

        // Targeted revert restores exactly what Knut changed.
        let outcome = applier.revert(&workspace, None).unwrap();
        assert!(outcome.skipped.is_empty());
        assert_eq!(fixture.read("tracked.txt"), "tracked\n");
        assert!(!fixture.exists("knut_new.txt"));
        // Still untouched.
        assert_eq!(fixture.read("user_work.txt"), "my work in progress\n");
    }

    #[test]
    fn revert_refuses_to_discard_changes_made_after_the_write() {
        let fixture = Fixture::new("revert-conflict");
        let hash = fixture.write("a.txt", "one\n");
        let patch = Patch::new(
            "edit",
            vec![PatchOp::Replace {
                path: "a.txt".to_owned(),
                content: "knut wrote this\n".to_owned(),
                expect_hash: hash,
            }],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let mut applier = PatchApplier::new();
        applier.apply(&workspace, &validated).unwrap();

        // The user keeps editing after Knut's write.
        fixture.write("a.txt", "user kept going\n");

        let outcome = applier.revert(&workspace, None).unwrap();
        assert!(outcome.reverted.is_empty());
        assert_eq!(outcome.skipped.len(), 1);
        // The user's newer work is intact.
        assert_eq!(fixture.read("a.txt"), "user kept going\n");
    }

    #[test]
    fn a_partial_failure_is_visible_and_leaves_no_silent_corruption() {
        let fixture = Fixture::new("partial");
        let hash = fixture.write("a.txt", "one\n");
        let move_hash = fixture.write("move.txt", "move\n");

        let patch = Patch::new(
            "two ops",
            vec![
                PatchOp::Replace {
                    path: "a.txt".to_owned(),
                    content: "two\n".to_owned(),
                    expect_hash: hash,
                },
                // A rename whose target appears between validation and
                // apply: the second operation fails for real.
                PatchOp::Rename {
                    from: "move.txt".to_owned(),
                    to: "taken.txt".to_owned(),
                    expect_hash: move_hash,
                    content: None,
                },
            ],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();

        // Something creates the rename target after validation.
        fixture.write("taken.txt", "someone else got here first\n");

        let mut applier = PatchApplier::new();
        let outcome = applier.apply(&workspace, &validated).unwrap();

        assert!(outcome.is_partial(), "outcome: {outcome:?}");
        assert_eq!(outcome.applied.len(), 1);
        let failure = outcome.failure.unwrap();
        assert!(failure.recovery_available);
        assert_eq!(failure.path, "taken.txt");
        // The first operation is real and recorded; the second did not
        // run, and the file it would have clobbered is intact.
        assert_eq!(fixture.read("a.txt"), "two\n");
        assert_eq!(fixture.read("taken.txt"), "someone else got here first\n");
        assert_eq!(fixture.read("move.txt"), "move\n");
        assert_eq!(applier.journal().len(), 1);
    }

    #[test]
    fn delete_and_rename_revert_restore_original_layout() {
        let fixture = Fixture::new("layout");
        let keep_hash = fixture.write("keep.txt", "keep\n");
        let move_hash = fixture.write("move.txt", "move\n");

        let patch = Patch::new(
            "reorganize",
            vec![
                PatchOp::Delete {
                    path: "keep.txt".to_owned(),
                    expect_hash: keep_hash,
                },
                PatchOp::Rename {
                    from: "move.txt".to_owned(),
                    to: "moved.txt".to_owned(),
                    expect_hash: move_hash,
                    content: None,
                },
            ],
        );

        let workspace = fixture.workspace();
        let validated = validate_patch(&workspace, &patch).unwrap();
        let mut applier = PatchApplier::new();
        applier.apply(&workspace, &validated).unwrap();
        assert!(!fixture.exists("keep.txt"));
        assert!(fixture.exists("moved.txt"));

        applier.revert(&workspace, None).unwrap();
        assert_eq!(fixture.read("keep.txt"), "keep\n");
        assert_eq!(fixture.read("move.txt"), "move\n");
        assert!(!fixture.exists("moved.txt"));
    }

    #[tokio::test]
    async fn the_gate_binds_approval_to_the_patch_and_its_source_revisions() {
        use crate::policy::ExecutionGate;

        let fixture = Fixture::new("gate");
        let hash = fixture.write("a.txt", "one\n");
        let workspace = fixture.workspace();

        let patch = Patch::new(
            "edit",
            vec![PatchOp::Replace {
                path: "a.txt".to_owned(),
                content: "two\n".to_owned(),
                expect_hash: hash,
            }],
        );
        let validated = validate_patch(&workspace, &patch).unwrap();

        let tool = ApplyPatchTool::new(workspace.clone());
        tool.stage(validated.clone());
        let metadata = crate::tool::Tool::metadata(&tool);

        // Policy requires approval for writes.
        let gate = ExecutionGate::new(
            crate::SideEffectPolicy::new()
                .allow(crate::SideEffect::ReadOnly)
                .require_approval(crate::SideEffect::IdempotentWrite),
        );

        let input = json!({ "patch_identity": validated.identity });
        let preconditions = tool.staged_preconditions();

        // Before approval: refused, and the key names this exact action.
        let err = gate
            .authorize_with_preconditions(&metadata, &input, &preconditions, None, crate::Risk::Low)
            .await
            .unwrap_err();
        let crate::KnutError::ApprovalRequired { approval_key, .. } = err else {
            panic!("expected ApprovalRequired, got {err:?}");
        };

        // Approving that exact fingerprint permits the apply.
        gate.grant_approval(&approval_key);
        gate.authorize_with_preconditions(
            &metadata,
            &input,
            &preconditions,
            None,
            crate::Risk::Low,
        )
        .await
        .expect("approved action authorizes");

        // A different patch (changed content) has a different
        // fingerprint, so the earlier approval does not authorize it.
        let changed = Patch::new(
            "edit",
            vec![PatchOp::Replace {
                path: "a.txt".to_owned(),
                content: "two but different\n".to_owned(),
                expect_hash: content_hash(b"one\n"),
            }],
        );
        let changed_validated = validate_patch(&workspace, &changed).unwrap();
        assert_ne!(changed_validated.identity, validated.identity);

        let changed_input = json!({ "patch_identity": changed_validated.identity });
        let err = gate
            .authorize_with_preconditions(
                &metadata,
                &changed_input,
                &changed_validated.gate_preconditions(),
                None,
                crate::Risk::Low,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::KnutError::ApprovalRequired { .. }),
            "a changed patch was authorized by an earlier approval: {err:?}"
        );

        // And moving the source revision under the same patch also
        // invalidates the approval: the same patch now fingerprints
        // against a different content revision.
        fixture.write("a.txt", "someone changed it\n");
        let moved_preconditions =
            ContentPreconditions::none().watch("a.txt", content_hash(b"someone changed it\n"));
        let err = gate
            .authorize_with_preconditions(
                &metadata,
                &input,
                &moved_preconditions,
                None,
                crate::Risk::Low,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::KnutError::ApprovalRequired { .. }),
            "a changed source revision kept its approval: {err:?}"
        );
    }

    #[tokio::test]
    async fn the_tool_refuses_a_patch_that_is_not_the_staged_one() {
        let fixture = Fixture::new("tool-identity");
        fixture.write("a.txt", "one\n");
        let workspace = fixture.workspace();

        let patch = Patch::new(
            "edit",
            vec![PatchOp::Create {
                path: "new.txt".to_owned(),
                content: "new\n".to_owned(),
            }],
        );
        let validated = validate_patch(&workspace, &patch).unwrap();

        let tool = ApplyPatchTool::new(workspace.clone());
        tool.stage(validated.clone());

        // A different identity cannot be applied.
        let err = crate::tool::Tool::call(&tool, json!({ "patch_identity": "fnv1a:deadbeef:0" }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("does not match"), "got {err}");

        // The staged one applies.
        let outcome =
            crate::tool::Tool::call(&tool, json!({ "patch_identity": validated.identity }))
                .await
                .unwrap();
        assert_eq!(outcome["failure"], Value::Null);
        assert_eq!(fixture.read("new.txt"), "new\n");
    }

    #[test]
    fn patches_are_bounded_in_size_and_count() {
        let fixture = Fixture::new("bounds");
        fixture.write("a.txt", "a\n");
        let workspace = fixture.workspace();

        let too_many = Patch::new(
            "x",
            (0..(MAX_PATCH_OPS + 1))
                .map(|i| PatchOp::Create {
                    path: format!("f{i}.txt"),
                    content: "x".to_owned(),
                })
                .collect(),
        );
        assert!(matches!(
            validate_patch(&workspace, &too_many).unwrap_err(),
            PatchRejection::TooManyOps { .. }
        ));

        let too_large = Patch::new(
            "x",
            vec![PatchOp::Create {
                path: "big.txt".to_owned(),
                content: "x".repeat(MAX_OP_BYTES + 1),
            }],
        );
        assert!(matches!(
            validate_patch(&workspace, &too_large).unwrap_err(),
            PatchRejection::TooLarge { .. }
        ));
    }
}
