//! Persistent sessions and crash-safe recovery (issue #31).
//!
//! A local SQLite store holds versioned session events, artifact
//! references, task revisions and operation journal state, so real work
//! survives a crash or a terminal restart without repeating writes.
//!
//! The rules that matter:
//! - **intent is written before dispatch, outcome after.** An operation
//!   that was dispatched and never reported is *unknown*, and is
//!   reconciled with the user rather than assumed not to have run.
//! - **replaying a transcript rebuilds state only.** It never dispatches
//!   a historical tool call or provider request.
//! - **resume validates before continuing.** Workspace identity, policy
//!   revision and pending-approval scope are re-checked; a stale grant is
//!   never resurrected.
//! - **one writer per workspace.** A second process cannot claim the same
//!   operation reservation.
//! - **corruption is reported, never silently reset.**
//!
//! Confidential payloads stay local: events are stored separately from
//! credentials (which are never stored at all), the database file is
//! created with owner-only permissions, and exports redact paths to
//! hashes by default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::KnutError;
use crate::session::SessionEvent;

/// Current schema version. Migrations are applied forward only.
pub const SCHEMA_VERSION: i64 = 1;

/// Whether an operation ever finished, and how.
///
/// Mirrors the gate's own states so a reconciled operation agrees with the
/// execution journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedOutcome {
    /// Intent recorded, dispatch attempted, outcome never observed.
    UnknownEffect,
    /// The operation completed and its output was stored.
    Completed,
    /// The operation failed before any effect.
    Failed,
}

impl PersistedOutcome {
    fn as_str(self) -> &'static str {
        match self {
            PersistedOutcome::UnknownEffect => "unknown_effect",
            PersistedOutcome::Completed => "completed",
            PersistedOutcome::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "unknown_effect" => Some(PersistedOutcome::UnknownEffect),
            "completed" => Some(PersistedOutcome::Completed),
            "failed" => Some(PersistedOutcome::Failed),
            _ => None,
        }
    }
}

/// One operation record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecord {
    /// The action fingerprint the gate uses.
    pub fingerprint: String,
    pub capability: String,
    pub tool_id: String,
    pub outcome: PersistedOutcome,
    /// Bounded output or diagnostics.
    pub output: Option<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

/// A resumable session summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub workspace: String,
    pub workspace_revision: String,
    pub policy_revision: String,
    pub task_state: String,
    pub event_count: usize,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Why a session cannot be resumed as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeBlocker {
    /// The workspace content changed since the session ran.
    WorkspaceChanged { recorded: String, current: String },
    /// The policy changed, so earlier approvals are stale.
    PolicyChanged { recorded: String, current: String },
    /// A pending approval whose scope can no longer be honoured.
    StaleApproval { approval_key: String },
    /// A dispatched operation whose outcome was never observed.
    UnreconciledOperation {
        fingerprint: String,
        tool_id: String,
    },
    /// The session was already finished.
    AlreadyFinished { state: String },
}

impl std::fmt::Display for ResumeBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResumeBlocker::WorkspaceChanged { recorded, current } => write!(
                f,
                "the workspace changed since this session (recorded {recorded}, now {current})"
            ),
            ResumeBlocker::PolicyChanged { recorded, current } => write!(
                f,
                "the policy changed since this session (recorded {recorded}, now {current}); \
                 earlier approvals are not valid"
            ),
            ResumeBlocker::StaleApproval { approval_key } => write!(
                f,
                "pending approval {approval_key} no longer applies to the current workspace"
            ),
            ResumeBlocker::UnreconciledOperation {
                fingerprint,
                tool_id,
            } => write!(
                f,
                "operation {tool_id} ({fingerprint}) was dispatched but its outcome is unknown; \
                 it needs reconciliation before resuming"
            ),
            ResumeBlocker::AlreadyFinished { state } => {
                write!(f, "the session already finished as {state}")
            }
        }
    }
}

/// A persisted session store.
pub struct SessionStore {
    connection: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("path", &self.path)
            .finish()
    }
}

impl SessionStore {
    /// Open (or create) a store at `path`.
    ///
    /// The file is created with owner-only permissions; a credential is
    /// never written here.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, KnutError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| KnutError::Tool(format!("creating {}: {err}", parent.display())))?;
        }

        let is_new = !path.exists();
        let connection = Connection::open(&path)
            .map_err(|err| KnutError::Tool(format!("opening {}: {err}", path.display())))?;

        if is_new {
            restrict_permissions(&path)?;
        }

        let mut store = Self { connection, path };
        store.migrate()?;
        Ok(store)
    }

    /// An in-memory store, for tests and dry runs.
    pub fn in_memory() -> Result<Self, KnutError> {
        let connection = Connection::open_in_memory()
            .map_err(|err| KnutError::Tool(format!("opening in-memory store: {err}")))?;
        let mut store = Self {
            connection,
            path: PathBuf::from(":memory:"),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create or upgrade the schema.
    ///
    /// An unsupported *newer* schema is reported rather than downgraded:
    /// silently discarding a newer store would be data loss.
    fn migrate(&mut self) -> Result<(), KnutError> {
        // A truncated or non-SQLite file fails here with a useful error.
        let integrity: String = self
            .connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|err| {
                KnutError::Tool(format!(
                    "{} is not a readable session store: {err}",
                    self.path.display()
                ))
            })?;
        if integrity != "ok" {
            return Err(KnutError::Tool(format!(
                "{} failed its integrity check: {integrity}",
                self.path.display()
            )));
        }

        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0);

        if version > SCHEMA_VERSION {
            return Err(KnutError::Tool(format!(
                "session store schema v{version} is newer than this build supports (v{SCHEMA_VERSION}); \
                 refusing to modify it"
            )));
        }

        if version == 0 {
            self.connection
                .execute_batch(
                    r#"
                    PRAGMA journal_mode = WAL;
                    PRAGMA synchronous = FULL;

                    CREATE TABLE IF NOT EXISTS sessions (
                        id TEXT PRIMARY KEY,
                        workspace TEXT NOT NULL,
                        workspace_revision TEXT NOT NULL,
                        policy_revision TEXT NOT NULL,
                        task_state TEXT NOT NULL,
                        created_at_ms INTEGER NOT NULL,
                        updated_at_ms INTEGER NOT NULL
                    );

                    CREATE TABLE IF NOT EXISTS events (
                        session_id TEXT NOT NULL,
                        sequence INTEGER NOT NULL,
                        payload TEXT NOT NULL,
                        PRIMARY KEY (session_id, sequence)
                    );

                    CREATE TABLE IF NOT EXISTS operations (
                        fingerprint TEXT PRIMARY KEY,
                        session_id TEXT NOT NULL,
                        capability TEXT NOT NULL,
                        tool_id TEXT NOT NULL,
                        outcome TEXT NOT NULL,
                        output TEXT,
                        started_at_ms INTEGER NOT NULL,
                        finished_at_ms INTEGER
                    );

                    CREATE TABLE IF NOT EXISTS artifacts (
                        session_id TEXT NOT NULL,
                        path TEXT NOT NULL,
                        content_hash TEXT NOT NULL,
                        PRIMARY KEY (session_id, path)
                    );

                    CREATE TABLE IF NOT EXISTS pending_approvals (
                        session_id TEXT NOT NULL,
                        approval_key TEXT NOT NULL,
                        scope TEXT NOT NULL,
                        PRIMARY KEY (session_id, approval_key)
                    );

                    CREATE TABLE IF NOT EXISTS locks (
                        workspace TEXT PRIMARY KEY,
                        session_id TEXT NOT NULL,
                        acquired_at_ms INTEGER NOT NULL
                    );
                    "#,
                )
                .map_err(|err| KnutError::Tool(format!("creating schema: {err}")))?;
            self.connection
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
                .map_err(|err| KnutError::Tool(format!("setting schema version: {err}")))?;
        }

        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64, KnutError> {
        self.connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|err| KnutError::Tool(format!("reading schema version: {err}")))
    }

    /// Claim the workspace for this session.
    ///
    /// Two processes must not both believe they own the same operation
    /// reservations, so the claim is exclusive per workspace.
    pub fn claim_workspace(
        &mut self,
        workspace: impl Into<String>,
        session_id: impl Into<String>,
        now_ms: u64,
    ) -> Result<(), KnutError> {
        let workspace = workspace.into();
        let session_id = session_id.into();

        if let Some(existing) = self.workspace_owner(&workspace)? {
            if existing != session_id {
                // A live owner process is the real conflict; a stale claim
                // from a dead session is reclaimable.
                if self.owner_is_live(&existing)? {
                    return Err(KnutError::Tool(format!(
                        "workspace {workspace:?} is already owned by live session {existing}; \
                         refusing to run two writers on one workspace"
                    )));
                }
                self.release_workspace(&workspace)?;
            } else {
                return Ok(());
            }
        }

        self.connection
            .execute(
                "INSERT INTO locks (workspace, session_id, acquired_at_ms) VALUES (?1, ?2, ?3)",
                params![workspace, session_id, now_ms as i64],
            )
            .map_err(|err| KnutError::Tool(format!("claiming workspace: {err}")))?;
        Ok(())
    }

    fn workspace_owner(&self, workspace: &str) -> Result<Option<String>, KnutError> {
        self.connection
            .query_row(
                "SELECT session_id FROM locks WHERE workspace = ?1",
                params![workspace],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| KnutError::Tool(format!("reading workspace lock: {err}")))
    }

    /// Whether the recorded owner session is still unfinished.
    fn owner_is_live(&self, session_id: &str) -> Result<bool, KnutError> {
        let state: Option<String> = self
            .connection
            .query_row(
                "SELECT task_state FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| KnutError::Tool(format!("reading session state: {err}")))?;
        Ok(matches!(
            state.as_deref(),
            Some("running") | Some("waiting") | Some("paused") | Some("queued")
        ))
    }

    pub fn release_workspace(&mut self, workspace: &str) -> Result<(), KnutError> {
        self.connection
            .execute("DELETE FROM locks WHERE workspace = ?1", params![workspace])
            .map_err(|err| KnutError::Tool(format!("releasing workspace: {err}")))?;
        Ok(())
    }

    /// Create or update a session row.
    #[allow(clippy::too_many_arguments)]
    pub fn start_session(
        &mut self,
        id: &str,
        workspace: &str,
        workspace_revision: &str,
        policy_revision: &str,
        task_state: &str,
        now_ms: u64,
    ) -> Result<(), KnutError> {
        self.connection
            .execute(
                "INSERT INTO sessions (id, workspace, workspace_revision, policy_revision, \
                 task_state, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
                 ON CONFLICT(id) DO UPDATE SET workspace_revision = ?3, policy_revision = ?4, \
                 task_state = ?5, updated_at_ms = ?6",
                params![
                    id,
                    workspace,
                    workspace_revision,
                    policy_revision,
                    task_state,
                    now_ms as i64
                ],
            )
            .map_err(|err| KnutError::Tool(format!("starting session: {err}")))?;
        Ok(())
    }

    /// Set the session's task state.
    pub fn set_task_state(&mut self, id: &str, state: &str, now_ms: u64) -> Result<(), KnutError> {
        self.connection
            .execute(
                "UPDATE sessions SET task_state = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![id, state, now_ms as i64],
            )
            .map_err(|err| KnutError::Tool(format!("updating task state: {err}")))?;
        Ok(())
    }

    /// Append one session event.
    ///
    /// Events are the transcript: replaying them rebuilds UI state only.
    pub fn append_event(
        &mut self,
        session_id: &str,
        event: &SessionEvent,
        now_ms: u64,
    ) -> Result<u64, KnutError> {
        let payload = serde_json::to_string(event)
            .map_err(|err| KnutError::Tool(format!("serializing event: {err}")))?;
        let next: i64 = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|err| KnutError::Tool(format!("reading event sequence: {err}")))?;

        self.connection
            .execute(
                "INSERT INTO events (session_id, sequence, payload) VALUES (?1, ?2, ?3)",
                params![session_id, next, payload],
            )
            .map_err(|err| KnutError::Tool(format!("appending event: {err}")))?;
        self.connection
            .execute(
                "UPDATE sessions SET updated_at_ms = ?2 WHERE id = ?1",
                params![session_id, now_ms as i64],
            )
            .ok();
        Ok(next as u64)
    }

    /// Load a session's events in order.
    pub fn events(&self, session_id: &str) -> Result<Vec<SessionEvent>, KnutError> {
        let mut statement = self
            .connection
            .prepare("SELECT sequence, payload FROM events WHERE session_id = ?1 ORDER BY sequence")
            .map_err(|err| KnutError::Tool(format!("reading events: {err}")))?;
        let rows = statement
            .query_map(params![session_id], |row| {
                let sequence: i64 = row.get(0)?;
                let payload: String = row.get(1)?;
                Ok((sequence, payload))
            })
            .map_err(|err| KnutError::Tool(format!("querying events: {err}")))?;

        let mut events = Vec::new();
        for row in rows {
            let (sequence, payload) =
                row.map_err(|err| KnutError::Tool(format!("reading a row: {err}")))?;
            // A corrupt transcript is reported with its position, never
            // silently skipped: losing events would make replay a lie.
            let event: SessionEvent = serde_json::from_str(&payload).map_err(|err| {
                KnutError::Tool(format!(
                    "event {sequence} in session {session_id} is unreadable: {err}"
                ))
            })?;
            events.push(event);
        }
        Ok(events)
    }

    /// List sessions, newest first.
    pub fn sessions(&self) -> Result<Vec<SessionSummary>, KnutError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT s.id, s.workspace, s.workspace_revision, s.policy_revision, s.task_state, \
                 s.created_at_ms, s.updated_at_ms, \
                 (SELECT COUNT(*) FROM events e WHERE e.session_id = s.id) \
                 FROM sessions s ORDER BY s.updated_at_ms DESC",
            )
            .map_err(|err| KnutError::Tool(format!("listing sessions: {err}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok(SessionSummary {
                    id: row.get(0)?,
                    workspace: row.get(1)?,
                    workspace_revision: row.get(2)?,
                    policy_revision: row.get(3)?,
                    task_state: row.get(4)?,
                    created_at_ms: row.get::<_, i64>(5)? as u64,
                    updated_at_ms: row.get::<_, i64>(6)? as u64,
                    event_count: row.get::<_, i64>(7)? as usize,
                })
            })
            .map_err(|err| KnutError::Tool(format!("querying sessions: {err}")))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|err| KnutError::Tool(format!("reading a row: {err}")))?);
        }
        Ok(sessions)
    }

    /// Fork a session: a new session id with a copy of the transcript.
    ///
    /// A fork is a *conversation* fork. It grants no filesystem rollback
    /// and starts no work.
    pub fn fork_session(
        &mut self,
        source: &str,
        new_id: &str,
        now_ms: u64,
    ) -> Result<(), KnutError> {
        let summary = self
            .sessions()?
            .into_iter()
            .find(|session| session.id == source)
            .ok_or_else(|| KnutError::Tool(format!("no session {source:?}")))?;

        self.start_session(
            new_id,
            &summary.workspace,
            &summary.workspace_revision,
            &summary.policy_revision,
            "queued",
            now_ms,
        )?;
        let events = self.events(source)?;
        for event in &events {
            self.append_event(new_id, event, now_ms)?;
        }
        Ok(())
    }

    /// Record the intent to run an operation, *before* dispatch.
    ///
    /// This is the durability point that makes an interrupted operation
    /// recoverable: the record exists even if the process dies mid-call.
    pub fn record_intent(
        &mut self,
        session_id: &str,
        fingerprint: &str,
        capability: &str,
        tool_id: &str,
        now_ms: u64,
    ) -> Result<(), KnutError> {
        self.connection
            .execute(
                "INSERT OR REPLACE INTO operations \
                 (fingerprint, session_id, capability, tool_id, outcome, output, started_at_ms, finished_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, 'unknown_effect', NULL, ?5, NULL)",
                params![fingerprint, session_id, capability, tool_id, now_ms as i64],
            )
            .map_err(|err| KnutError::Tool(format!("recording intent: {err}")))?;
        Ok(())
    }

    /// Record the observed outcome, *after* dispatch.
    pub fn record_outcome(
        &mut self,
        fingerprint: &str,
        outcome: PersistedOutcome,
        output: Option<&str>,
        now_ms: u64,
    ) -> Result<(), KnutError> {
        self.connection
            .execute(
                "UPDATE operations SET outcome = ?2, output = ?3, finished_at_ms = ?4 \
                 WHERE fingerprint = ?1",
                params![fingerprint, outcome.as_str(), output, now_ms as i64],
            )
            .map_err(|err| KnutError::Tool(format!("recording outcome: {err}")))?;
        Ok(())
    }

    /// Operations whose outcome was never observed.
    pub fn unreconciled_operations(&self) -> Result<Vec<OperationRecord>, KnutError> {
        self.operations_with(PersistedOutcome::UnknownEffect)
    }

    fn operations_with(
        &self,
        outcome: PersistedOutcome,
    ) -> Result<Vec<OperationRecord>, KnutError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT fingerprint, capability, tool_id, outcome, output, started_at_ms, finished_at_ms \
                 FROM operations WHERE outcome = ?1",
            )
            .map_err(|err| KnutError::Tool(format!("reading operations: {err}")))?;
        let rows = statement
            .query_map(params![outcome.as_str()], |row| {
                let text: String = row.get(3)?;
                Ok(OperationRecord {
                    fingerprint: row.get(0)?,
                    capability: row.get(1)?,
                    tool_id: row.get(2)?,
                    outcome: PersistedOutcome::parse(&text).unwrap_or(PersistedOutcome::Failed),
                    output: row.get(4)?,
                    started_at_ms: row.get::<_, i64>(5)? as u64,
                    finished_at_ms: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                })
            })
            .map_err(|err| KnutError::Tool(format!("querying operations: {err}")))?;

        let mut operations = Vec::new();
        for row in rows {
            operations.push(row.map_err(|err| KnutError::Tool(format!("reading a row: {err}")))?);
        }
        Ok(operations)
    }

    /// An operation record by fingerprint.
    pub fn operation(&self, fingerprint: &str) -> Result<Option<OperationRecord>, KnutError> {
        self.connection
            .query_row(
                "SELECT fingerprint, capability, tool_id, outcome, output, started_at_ms, finished_at_ms \
                 FROM operations WHERE fingerprint = ?1",
                params![fingerprint],
                |row| {
                    let text: String = row.get(3)?;
                    Ok(OperationRecord {
                        fingerprint: row.get(0)?,
                        capability: row.get(1)?,
                        tool_id: row.get(2)?,
                        outcome: PersistedOutcome::parse(&text)
                            .unwrap_or(PersistedOutcome::Failed),
                        output: row.get(4)?,
                        started_at_ms: row.get::<_, i64>(5)? as u64,
                        finished_at_ms: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    })
                },
            )
            .optional()
            .map_err(|err| KnutError::Tool(format!("reading operation: {err}")))
    }

    /// Record a pending approval's scope.
    pub fn record_pending_approval(
        &mut self,
        session_id: &str,
        approval_key: &str,
        scope: &str,
    ) -> Result<(), KnutError> {
        self.connection
            .execute(
                "INSERT OR REPLACE INTO pending_approvals (session_id, approval_key, scope) \
                 VALUES (?1, ?2, ?3)",
                params![session_id, approval_key, scope],
            )
            .map_err(|err| KnutError::Tool(format!("recording approval: {err}")))?;
        Ok(())
    }

    pub fn clear_pending_approval(
        &mut self,
        session_id: &str,
        approval_key: &str,
    ) -> Result<(), KnutError> {
        self.connection
            .execute(
                "DELETE FROM pending_approvals WHERE session_id = ?1 AND approval_key = ?2",
                params![session_id, approval_key],
            )
            .map_err(|err| KnutError::Tool(format!("clearing approval: {err}")))?;
        Ok(())
    }

    /// Pending approvals for a session.
    pub fn pending_approvals(&self, session_id: &str) -> Result<Vec<(String, String)>, KnutError> {
        let mut statement = self
            .connection
            .prepare("SELECT approval_key, scope FROM pending_approvals WHERE session_id = ?1")
            .map_err(|err| KnutError::Tool(format!("reading approvals: {err}")))?;
        let rows = statement
            .query_map(params![session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| KnutError::Tool(format!("querying approvals: {err}")))?;
        let mut approvals = Vec::new();
        for row in rows {
            approvals.push(row.map_err(|err| KnutError::Tool(format!("reading a row: {err}")))?);
        }
        Ok(approvals)
    }

    /// Record a file revision the session depends on.
    pub fn record_artifact(
        &mut self,
        session_id: &str,
        path: &str,
        content_hash: &str,
    ) -> Result<(), KnutError> {
        self.connection
            .execute(
                "INSERT OR REPLACE INTO artifacts (session_id, path, content_hash) \
                 VALUES (?1, ?2, ?3)",
                params![session_id, path, content_hash],
            )
            .map_err(|err| KnutError::Tool(format!("recording artifact: {err}")))?;
        Ok(())
    }

    pub fn artifacts(&self, session_id: &str) -> Result<BTreeMap<String, String>, KnutError> {
        let mut statement = self
            .connection
            .prepare("SELECT path, content_hash FROM artifacts WHERE session_id = ?1")
            .map_err(|err| KnutError::Tool(format!("reading artifacts: {err}")))?;
        let rows = statement
            .query_map(params![session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| KnutError::Tool(format!("querying artifacts: {err}")))?;
        let mut artifacts = BTreeMap::new();
        for row in rows {
            let (path, hash) =
                row.map_err(|err| KnutError::Tool(format!("reading a row: {err}")))?;
            artifacts.insert(path, hash);
        }
        Ok(artifacts)
    }

    /// Check whether a session can be resumed as-is.
    ///
    /// Every blocker is returned, so the caller can present the full
    /// picture instead of one error at a time.
    pub fn resume_blockers(
        &self,
        session_id: &str,
        current_workspace_revision: &str,
        current_policy_revision: &str,
    ) -> Result<Vec<ResumeBlocker>, KnutError> {
        let mut blockers = Vec::new();

        let Some(summary) = self
            .sessions()?
            .into_iter()
            .find(|session| session.id == session_id)
        else {
            return Err(KnutError::Tool(format!("no session {session_id:?}")));
        };

        if matches!(
            summary.task_state.as_str(),
            "completed" | "failed" | "cancelled"
        ) {
            blockers.push(ResumeBlocker::AlreadyFinished {
                state: summary.task_state.clone(),
            });
        }

        if summary.workspace_revision != current_workspace_revision {
            blockers.push(ResumeBlocker::WorkspaceChanged {
                recorded: summary.workspace_revision.clone(),
                current: current_workspace_revision.to_owned(),
            });
        }

        if summary.policy_revision != current_policy_revision {
            blockers.push(ResumeBlocker::PolicyChanged {
                recorded: summary.policy_revision.clone(),
                current: current_policy_revision.to_owned(),
            });
        }

        // A pending approval is only resurrected when its scope still
        // matches the workspace, which a changed revision above already
        // rules out; report it explicitly so the user sees why.
        for (approval_key, _scope) in self.pending_approvals(session_id)? {
            if summary.workspace_revision != current_workspace_revision
                || summary.policy_revision != current_policy_revision
            {
                blockers.push(ResumeBlocker::StaleApproval { approval_key });
            }
        }

        for operation in self.unreconciled_operations()? {
            blockers.push(ResumeBlocker::UnreconciledOperation {
                fingerprint: operation.fingerprint,
                tool_id: operation.tool_id,
            });
        }

        Ok(blockers)
    }

    /// Compact the store, keeping the state recovery needs.
    ///
    /// Cosmetic stream events are dropped beyond the retention window;
    /// operations, approvals and terminal events are kept, because those
    /// are exactly what recovery reads.
    pub fn compact(&mut self, session_id: &str, keep_events: usize) -> Result<usize, KnutError> {
        let total: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|err| KnutError::Tool(format!("counting events: {err}")))?;

        let keep = keep_events as i64;
        if total <= keep {
            return Ok(0);
        }

        let cutoff = total - keep;
        let removed = self
            .connection
            .execute(
                "DELETE FROM events WHERE session_id = ?1 AND sequence <= ?2 \
                 AND payload NOT LIKE '%\"task_completed\"%' \
                 AND payload NOT LIKE '%\"task_failed\"%' \
                 AND payload NOT LIKE '%\"task_cancelled\"%'",
                params![session_id, cutoff],
            )
            .map_err(|err| KnutError::Tool(format!("compacting events: {err}")))?;
        Ok(removed)
    }
}

/// Restrict a file to its owner.
fn restrict_permissions(path: &Path) -> Result<(), KnutError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, permissions)
            .map_err(|err| KnutError::Tool(format!("restricting {}: {err}", path.display())))?;
    }
    let _ = path;
    Ok(())
}

/// Replace raw paths with stable hashes for export.
///
/// Exported traces are shareable by default: private code and secrets are
/// not included unless the operator explicitly asks for raw content.
pub fn redact_path(path: &str) -> String {
    let hash = crate::workspace::content_hash(path.as_bytes());
    let digest = hash
        .split(':')
        .nth(1)
        .unwrap_or("0000000000000000")
        .to_owned();
    format!("file-{digest}")
}

/// What an export contains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionExport {
    pub session: SessionSummary,
    /// Events with paths redacted by default.
    pub events: Vec<String>,
    /// Operations, which are the audit-relevant part.
    pub operations: Vec<OperationRecord>,
    /// Artifact references as (redacted path, hash).
    pub artifacts: Vec<(String, String)>,
    /// Whether raw content was included.
    pub raw_paths_included: bool,
}

impl SessionStore {
    /// Export a session without executing anything.
    ///
    /// `include_raw_paths` is an explicit choice: the default redacts
    /// paths to hashes so a trace can be shared.
    pub fn export(
        &self,
        session_id: &str,
        include_raw_paths: bool,
    ) -> Result<SessionExport, KnutError> {
        let session = self
            .sessions()?
            .into_iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| KnutError::Tool(format!("no session {session_id:?}")))?;

        let events: Vec<String> = self
            .events(session_id)?
            .iter()
            .map(|event| {
                let text = serde_json::to_string(event).unwrap_or_default();
                if include_raw_paths {
                    text
                } else {
                    redact_paths_in(&text)
                }
            })
            .collect();

        // Every operation, whatever its state: the audit trail is the
        // point of an export.
        let operations = self
            .operations_with(PersistedOutcome::UnknownEffect)?
            .into_iter()
            .chain(self.operations_with(PersistedOutcome::Completed)?)
            .chain(self.operations_with(PersistedOutcome::Failed)?)
            .collect();

        let artifacts = self
            .artifacts(session_id)?
            .into_iter()
            .map(|(path, hash)| {
                if include_raw_paths {
                    (path, hash)
                } else {
                    (redact_path(&path), hash)
                }
            })
            .collect();

        Ok(SessionExport {
            session,
            events,
            operations,
            artifacts,
            raw_paths_included: include_raw_paths,
        })
    }
}

/// Redact path-like strings in a serialized event.
fn redact_paths_in(text: &str) -> String {
    // The transcript's own paths appear in a small set of fields; a
    // conservative scrub rewrites anything that looks like a filesystem
    // path while leaving identifiers intact.
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || "/._-".contains(c) {
            token.push(c);
        } else {
            out.push_str(&redact_token(&token));
            token.clear();
            out.push(c);
        }
    }
    out.push_str(&redact_token(&token));
    out
}

fn redact_token(token: &str) -> String {
    let looks_like_path = (token.contains('/')
        || token.ends_with(".rs")
        || token.ends_with(".toml")
        || token.ends_with(".ts"))
        && token.len() > 3
        && !token.starts_with("http");
    if looks_like_path {
        redact_path(token)
    } else {
        token.to_owned()
    }
}

/// A resume plan: what a caller may do with a stored session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePlan {
    /// Safe to continue: no blockers, and the transcript replays state
    /// only.
    Continue,
    /// Needs the user: unknown effects must be reconciled first.
    Reconcile(Vec<ResumeBlocker>),
    /// Cannot continue without the user re-approving or restarting.
    Refuse(Vec<ResumeBlocker>),
}

impl SessionStore {
    /// Decide how a stored session may be resumed.
    pub fn plan_resume(
        &self,
        session_id: &str,
        current_workspace_revision: &str,
        current_policy_revision: &str,
    ) -> Result<ResumePlan, KnutError> {
        let blockers = self.resume_blockers(
            session_id,
            current_workspace_revision,
            current_policy_revision,
        )?;
        if blockers.is_empty() {
            return Ok(ResumePlan::Continue);
        }
        // An unknown effect is recoverable *with the user*; everything
        // else means the session cannot simply continue.
        let needs_user = blockers
            .iter()
            .any(|blocker| matches!(blocker, ResumeBlocker::UnreconciledOperation { .. }));
        let hard_stop = blockers.iter().any(|blocker| {
            matches!(
                blocker,
                ResumeBlocker::WorkspaceChanged { .. }
                    | ResumeBlocker::PolicyChanged { .. }
                    | ResumeBlocker::StaleApproval { .. }
                    | ResumeBlocker::AlreadyFinished { .. }
            )
        });
        if hard_stop {
            Ok(ResumePlan::Refuse(blockers))
        } else if needs_user {
            Ok(ResumePlan::Reconcile(blockers))
        } else {
            Ok(ResumePlan::Refuse(blockers))
        }
    }
}

/// Rebuild workbench state from a stored transcript.
///
/// The reducer is pure, so replaying is state-only: nothing here can
/// dispatch a tool or a provider call.
pub fn replay_state(
    store: &SessionStore,
    session_id: &str,
    workspace: impl Into<String>,
) -> Result<crate::tui_state::WorkbenchState, KnutError> {
    let mut state = crate::tui_state::WorkbenchState::new(workspace);
    for event in store.events(session_id)? {
        state.apply(&event);
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TaskId;

    fn store() -> SessionStore {
        SessionStore::in_memory().expect("in-memory store")
    }

    fn event(prompt: &str) -> SessionEvent {
        SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: prompt.to_owned(),
        }
    }

    #[test]
    fn events_round_trip_in_order() {
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev-1", "pol-1", "running", 1_000)
            .unwrap();

        store.append_event("s1", &event("first"), 1_001).unwrap();
        store
            .append_event(
                "s1",
                &SessionEvent::TaskFailed {
                    task: TaskId(1),
                    reason: "boom".to_owned(),
                },
                1_002,
            )
            .unwrap();

        let events = store.events("s1").unwrap();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], SessionEvent::TaskStarted { prompt, .. } if prompt == "first")
        );
        assert!(matches!(&events[1], SessionEvent::TaskFailed { reason, .. } if reason == "boom"));
    }

    #[test]
    fn crash_states_are_distinct_and_recoverable() {
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev-1", "pol-1", "running", 1_000)
            .unwrap();

        // (a) Crash before dispatch: no intent recorded at all.
        assert!(store.operation("fp-before").unwrap().is_none());

        // (b) Crash during a command: intent recorded, outcome not.
        store
            .record_intent("s1", "fp-during", "shell", "run", 1_100)
            .unwrap();
        let record = store.operation("fp-during").unwrap().unwrap();
        assert_eq!(record.outcome, PersistedOutcome::UnknownEffect);
        assert!(record.finished_at_ms.is_none());

        // (c) Crash after the effect but before outcome persistence: the
        //     same as (b) from the store's point of view — which is
        //     exactly why it is reconciled rather than retried.
        let unreconciled = store.unreconciled_operations().unwrap();
        assert_eq!(unreconciled.len(), 1);
        assert_eq!(unreconciled[0].tool_id, "run");

        // (d) Outcome persisted: distinguishable from both.
        store
            .record_outcome(
                "fp-during",
                PersistedOutcome::Completed,
                Some("{\"ok\":true}"),
                1_200,
            )
            .unwrap();
        let record = store.operation("fp-during").unwrap().unwrap();
        assert_eq!(record.outcome, PersistedOutcome::Completed);
        assert!(record.finished_at_ms.is_some());
        assert!(store.unreconciled_operations().unwrap().is_empty());

        // A failed outcome is its own state.
        store
            .record_intent("s1", "fp-failed", "files", "apply_patch", 1_300)
            .unwrap();
        store
            .record_outcome("fp-failed", PersistedOutcome::Failed, Some("stale"), 1_400)
            .unwrap();
        assert_eq!(
            store.operation("fp-failed").unwrap().unwrap().outcome,
            PersistedOutcome::Failed
        );
    }

    #[test]
    fn an_unknown_effect_needs_reconciliation_before_resuming() {
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev-1", "pol-1", "running", 1_000)
            .unwrap();
        store
            .record_intent("s1", "fp", "files", "apply_patch", 1_100)
            .unwrap();

        let plan = store.plan_resume("s1", "rev-1", "pol-1").unwrap();
        match plan {
            ResumePlan::Reconcile(blockers) => {
                assert!(
                    blockers.iter().any(|blocker| matches!(
                        blocker,
                        ResumeBlocker::UnreconciledOperation { .. }
                    ))
                );
            }
            other => panic!("expected Reconcile, got {other:?}"),
        }

        // Once reconciled, the session resumes cleanly.
        store
            .record_outcome("fp", PersistedOutcome::Completed, None, 1_200)
            .unwrap();
        assert_eq!(
            store.plan_resume("s1", "rev-1", "pol-1").unwrap(),
            ResumePlan::Continue
        );
    }

    #[test]
    fn replaying_a_session_dispatches_nothing() {
        // The transcript replays state only: a completed write in the
        // transcript produces a UI entry, never a second invocation.
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev-1", "pol-1", "running", 1_000)
            .unwrap();
        store
            .append_event("s1", &event("apply the patch"), 1_001)
            .unwrap();
        store
            .record_intent("s1", "fp", "files", "apply_patch", 1_002)
            .unwrap();
        store
            .record_outcome("fp", PersistedOutcome::Completed, None, 1_003)
            .unwrap();
        store
            .append_event(
                "s1",
                &SessionEvent::TaskCompleted {
                    task: TaskId(1),
                    summary: "applied".to_owned(),
                },
                1_004,
            )
            .unwrap();

        let state = replay_state(&store, "s1", "/ws").unwrap();
        // The state reflects the transcript.
        assert_eq!(state.task_state, Some(crate::session::TaskState::Completed));
        // And nothing was dispatched: the operation row is unchanged and
        // there is no new intent.
        assert_eq!(
            store.operation("fp").unwrap().unwrap().outcome,
            PersistedOutcome::Completed
        );
        assert!(store.unreconciled_operations().unwrap().is_empty());
    }

    #[test]
    fn a_changed_workspace_or_policy_invalidates_resume_and_approvals() {
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev-1", "pol-1", "waiting", 1_000)
            .unwrap();
        store
            .record_pending_approval("s1", "approval-key", "files:apply_patch")
            .unwrap();

        // The workspace changed: the approval is stale and resume refuses.
        let plan = store.plan_resume("s1", "rev-2", "pol-1").unwrap();
        match plan {
            ResumePlan::Refuse(blockers) => {
                assert!(
                    blockers
                        .iter()
                        .any(|blocker| matches!(blocker, ResumeBlocker::WorkspaceChanged { .. }))
                );
                assert!(
                    blockers
                        .iter()
                        .any(|blocker| matches!(blocker, ResumeBlocker::StaleApproval { .. }))
                );
            }
            other => panic!("expected Refuse, got {other:?}"),
        }

        // The policy changed: same outcome, different reason.
        let plan = store.plan_resume("s1", "rev-1", "pol-2").unwrap();
        match plan {
            ResumePlan::Refuse(blockers) => {
                assert!(
                    blockers
                        .iter()
                        .any(|blocker| matches!(blocker, ResumeBlocker::PolicyChanged { .. }))
                );
            }
            other => panic!("expected Refuse, got {other:?}"),
        }

        // Unchanged: it may continue.
        assert_eq!(
            store.plan_resume("s1", "rev-1", "pol-1").unwrap(),
            ResumePlan::Continue
        );
    }

    #[test]
    fn a_finished_session_is_not_resumed() {
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev-1", "pol-1", "completed", 1_000)
            .unwrap();
        let plan = store.plan_resume("s1", "rev-1", "pol-1").unwrap();
        assert!(matches!(plan, ResumePlan::Refuse(_)));
    }

    #[test]
    fn one_writer_owns_a_workspace() {
        let mut store = store();
        store.claim_workspace("/ws", "s1", 1_000).unwrap();

        // A live session keeps the claim.
        store
            .start_session("s1", "/ws", "rev", "pol", "running", 1_000)
            .unwrap();
        let err = store.claim_workspace("/ws", "s2", 1_001).unwrap_err();
        assert!(format!("{err}").contains("already owned"), "got {err}");

        // Once the owner finished, the claim is reclaimable.
        store.set_task_state("s1", "completed", 1_002).unwrap();
        store.claim_workspace("/ws", "s2", 1_003).unwrap();
        assert_eq!(store.workspace_owner("/ws").unwrap().unwrap(), "s2");
    }

    #[test]
    fn forking_copies_the_transcript_and_grants_no_rollback() {
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev-1", "pol-1", "running", 1_000)
            .unwrap();
        store.append_event("s1", &event("original"), 1_001).unwrap();
        store
            .record_artifact("s1", "src/a.rs", "fnv1a:abc")
            .unwrap();

        store.fork_session("s1", "s2", 2_000).unwrap();

        // The fork has the transcript...
        let events = store.events("s2").unwrap();
        assert_eq!(events.len(), 1);
        // ...and starts with no pending approvals of its own.
        assert!(store.pending_approvals("s2").unwrap().is_empty());
        // Both sessions still exist; nothing was rolled back.
        assert_eq!(store.sessions().unwrap().len(), 2);
    }

    #[test]
    fn a_corrupt_store_or_transcript_reports_instead_of_resetting() {
        // A file that is not SQLite at all.
        let dir = std::env::temp_dir().join(format!(
            "knut-store-corrupt-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");
        std::fs::write(&path, b"this is not a database, it is a truncation").unwrap();

        let err = SessionStore::open(&path).unwrap_err();
        assert!(
            format!("{err}").contains("not a readable session store"),
            "got {err}"
        );
        // The file was not deleted or rewritten.
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);

        // A transcript row that does not parse is reported with position.
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev", "pol", "running", 1_000)
            .unwrap();
        store.append_event("s1", &event("ok"), 1_001).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO events (session_id, sequence, payload) VALUES ('s1', 99, 'not json')",
                [],
            )
            .unwrap();
        let err = store.events("s1").unwrap_err();
        assert!(format!("{err}").contains("event 99"), "got {err}");
    }

    #[test]
    fn a_newer_schema_is_refused_not_downgraded() {
        let dir = std::env::temp_dir().join(format!(
            "knut-store-newer-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");

        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 5))
                .unwrap();
        }

        let err = SessionStore::open(&path).unwrap_err();
        assert!(
            format!("{err}").contains("newer than this build"),
            "got {err}"
        );
        // The store is untouched: its version is still the newer one.
        {
            let connection = Connection::open(&path).unwrap();
            let version: i64 = connection
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION + 5);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_redacts_paths_by_default_and_runs_nothing() {
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev", "pol", "running", 1_000)
            .unwrap();
        store
            .append_event(
                "s1",
                &SessionEvent::TaskFailed {
                    task: TaskId(1),
                    reason: "could not read /home/user/secret-project/src/lib.rs".to_owned(),
                },
                1_001,
            )
            .unwrap();
        store
            .record_intent("s1", "fp", "files", "read", 1_002)
            .unwrap();
        store
            .record_outcome("fp", PersistedOutcome::Completed, None, 1_003)
            .unwrap();
        store
            .record_artifact("s1", "src/a.rs", "fnv1a:abc")
            .unwrap();

        let export = store.export("s1", false).unwrap();
        assert!(!export.raw_paths_included);
        let joined = export.events.join(" ");
        // The raw path is not in the export.
        assert!(
            !joined.contains("secret-project"),
            "export leaked a path: {joined}"
        );
        assert!(!joined.contains("src/lib.rs"));
        assert!(joined.contains("file-"));
        // Artifact paths are redacted too.
        assert!(
            export
                .artifacts
                .iter()
                .all(|(path, _)| path.starts_with("file-"))
        );
        // The audit-relevant operation record is present.
        assert!(export.operations.iter().any(|op| op.tool_id == "read"));

        // Exporting again does not dispatch anything.
        let before = store.unreconciled_operations().unwrap().len();
        let _ = store.export("s1", false).unwrap();
        assert_eq!(store.unreconciled_operations().unwrap().len(), before);

        // The explicit choice does include raw paths.
        let raw = store.export("s1", true).unwrap();
        assert!(raw.raw_paths_included);
        assert!(raw.events.join(" ").contains("src/lib.rs"));
    }

    #[test]
    fn compaction_keeps_what_recovery_needs() {
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev", "pol", "running", 1_000)
            .unwrap();

        // Many cosmetic streaming events...
        for i in 0..500 {
            store
                .append_event(
                    "s1",
                    &SessionEvent::TextDelta {
                        task: TaskId(1),
                        turn: crate::session::TurnId(1),
                        node: crate::session::NodeId(1),
                        text: format!("chunk {i}"),
                    },
                    1_000 + i,
                )
                .unwrap();
        }
        // ...and one terminal event, which must survive.
        store
            .append_event(
                "s1",
                &SessionEvent::TaskCompleted {
                    task: TaskId(1),
                    summary: "done".to_owned(),
                },
                2_000,
            )
            .unwrap();
        store
            .record_intent("s1", "fp", "files", "apply_patch", 2_001)
            .unwrap();

        let removed = store.compact("s1", 20).unwrap();
        assert!(removed > 0);

        let events = store.events("s1").unwrap();
        // The terminal event survived compaction.
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::TaskCompleted { .. }))
        );
        // And the operation state recovery depends on is untouched.
        assert!(store.operation("fp").unwrap().is_some());
        assert_eq!(store.unreconciled_operations().unwrap().len(), 1);
    }

    #[test]
    fn the_store_file_is_owner_only() {
        let dir = std::env::temp_dir().join(format!(
            "knut-store-perms-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");
        let _store = SessionStore::open(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "store is not owner-only: {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_version_is_reported() {
        let store = store();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }
}
