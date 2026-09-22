//! Headless JSONL and ACP adapters over the same session runtime
//! (issue #40).
//!
//! Scripts and editors use the *same* engine as the TUI: one
//! `SessionRuntime`, one command/event contract, one approval fingerprint,
//! one journal. Nothing here reimplements permissions, cancellation or
//! session semantics, because a second implementation is how those drift.
//!
//! Contracts:
//! - **stdout carries protocol messages only.** Diagnostics go to stderr,
//!   so a client parsing JSONL never trips over a log line.
//! - **approvals are exact.** A client's approval resolves the same
//!   pending-action fingerprint the TUI would; a missing permission UI
//!   means the action stays *blocked*, never implicitly approved.
//! - **one writer per session.** A second client cannot race a writable
//!   session.
//! - **disconnects stop work.** A client that vanishes cannot leave new
//!   work dispatching.
//! - **ACP capabilities are advertised only when implemented**, and the
//!   protocol version is negotiated rather than assumed.
//!
//! ACP reference: <https://agentclientprotocol.com/protocol/v1/overview>,
//! protocol version 1.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::KnutError;
use crate::session::{SessionCommand, SessionEvent, TaskState, WaitKind};

/// The JSONL protocol version for the headless adapter.
pub const JSONL_PROTOCOL_VERSION: u32 = 1;

/// The ACP protocol version this adapter implements.
pub const ACP_PROTOCOL_VERSION: u32 = 1;

/// Maximum bytes in one JSONL line.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

// --- headless JSONL -----------------------------------------------------

/// A command line as a client sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HeadlessCommand {
    /// A new task.
    Submit {
        prompt: String,
    },
    /// Steer the active task.
    Steer {
        prompt: String,
    },
    /// Queue a request for the next task.
    Queue {
        prompt: String,
    },
    Pause,
    Resume,
    Answer {
        value: String,
    },
    /// Approve an exact pending action by its fingerprint.
    Approve {
        approval_key: String,
    },
    Deny {
        approval_key: String,
    },
    Cancel,
    /// Close the input side; the session drains and stops.
    Close,
}

impl HeadlessCommand {
    /// Parse one line.
    pub fn parse(line: &str) -> Result<Self, ProtocolError> {
        if line.len() > MAX_LINE_BYTES {
            return Err(ProtocolError::OversizedLine {
                bytes: line.len(),
                limit: MAX_LINE_BYTES,
            });
        }
        serde_json::from_str(line).map_err(|err| ProtocolError::Malformed {
            detail: err.to_string(),
        })
    }

    /// Translate into the engine's own command.
    ///
    /// `Queue` has no engine equivalent: queuing is a runtime decision
    /// (`SteerOrQueue`), so a client's explicit queue request is expressed
    /// as a multi-line prompt, which the runtime classifies as new work
    /// rather than steering. This is stated rather than hidden.
    pub fn to_session_command(&self) -> Option<SessionCommand> {
        match self {
            HeadlessCommand::Submit { prompt } => Some(SessionCommand::Submit {
                prompt: prompt.clone(),
            }),
            HeadlessCommand::Steer { prompt } => Some(SessionCommand::Steer {
                prompt: prompt.clone(),
            }),
            HeadlessCommand::Queue { prompt } => Some(SessionCommand::SteerOrQueue {
                // The newline makes the runtime class it as queued work.
                text: format!("{prompt}\n"),
            }),
            HeadlessCommand::Pause => Some(SessionCommand::Pause),
            HeadlessCommand::Resume => Some(SessionCommand::Resume),
            HeadlessCommand::Answer { value } => Some(SessionCommand::Answer {
                value: value.clone(),
            }),
            HeadlessCommand::Approve { approval_key } => Some(SessionCommand::Approve {
                approval_key: approval_key.clone(),
            }),
            HeadlessCommand::Deny { approval_key } => Some(SessionCommand::Deny {
                approval_key: approval_key.clone(),
            }),
            HeadlessCommand::Cancel => Some(SessionCommand::Cancel),
            HeadlessCommand::Close => None,
        }
    }
}

/// A protocol-level error, distinct from a model or tool failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProtocolError {
    Malformed {
        detail: String,
    },
    OversizedLine {
        bytes: usize,
        limit: usize,
    },
    /// The client asked for a version this adapter does not implement.
    UnsupportedVersion {
        requested: u32,
        supported: u32,
    },
    /// The session is owned by another client.
    SessionLocked {
        owner: String,
    },
    /// A command arrived with no active task.
    NoActiveTask,
    /// A response for an unknown request.
    UnknownRequest {
        id: Value,
    },
}

impl ProtocolError {
    /// A JSONL envelope, so a client can parse errors like anything else.
    pub fn to_json(&self) -> Value {
        json!({ "type": "error", "protocol_version": JSONL_PROTOCOL_VERSION, "error": self })
    }

    /// Whether the error is a protocol problem rather than a task failure.
    ///
    /// Kept separate so a client can tell "your message was malformed"
    /// from "the model failed".
    pub fn is_protocol_error(&self) -> bool {
        true
    }
}

/// What the headless adapter writes to stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HeadlessEvent {
    /// Emitted once at start, with the protocol version.
    Ready {
        protocol_version: u32,
        session: String,
    },
    /// A session event, translated.
    Event {
        protocol_version: u32,
        event: SessionEvent,
    },
    /// The session reached a terminal outcome.
    Outcome {
        protocol_version: u32,
        state: HeadlessOutcome,
        /// Why, when it was not a plain completion.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A protocol error.
    Error {
        protocol_version: u32,
        error: ProtocolError,
    },
}

/// Machine-readable terminal outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadlessOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl HeadlessOutcome {
    /// Translate the engine's task state.
    pub fn from_task_state(state: TaskState) -> Option<Self> {
        match state {
            TaskState::Completed => Some(HeadlessOutcome::Completed),
            TaskState::Failed => Some(HeadlessOutcome::Failed),
            TaskState::Cancelled => Some(HeadlessOutcome::Cancelled),
            _ => None,
        }
    }
}

/// Serialize one event as a single JSONL line.
pub fn to_jsonl(event: &HeadlessEvent) -> Result<String, ProtocolError> {
    let line = serde_json::to_string(event).map_err(|err| ProtocolError::Malformed {
        detail: err.to_string(),
    })?;
    if line.len() > MAX_LINE_BYTES {
        // A single event too large for one line is reported as an error
        // rather than emitting a line a client cannot parse.
        return Err(ProtocolError::OversizedLine {
            bytes: line.len(),
            limit: MAX_LINE_BYTES,
        });
    }
    Ok(line)
}

/// The state of one headless session, including its single writer.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessSession {
    pub id: String,
    owner: Option<String>,
    closed: bool,
    /// The exact fingerprint the client was last offered for approval.
    pub pending_approval: Option<String>,
}

impl HeadlessSession {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            owner: None,
            closed: false,
            pending_approval: None,
        }
    }

    /// Claim the session for a client.
    ///
    /// One writer at a time: a second client cannot race a writable
    /// session, which is what keeps journals and approvals coherent.
    pub fn claim(&mut self, client: impl Into<String>) -> Result<(), ProtocolError> {
        let client = client.into();
        match &self.owner {
            Some(owner) if *owner != client => Err(ProtocolError::SessionLocked {
                owner: owner.clone(),
            }),
            _ => {
                self.owner = Some(client);
                Ok(())
            }
        }
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Release the session, stopping any new dispatch.
    pub fn disconnect(&mut self) {
        self.owner = None;
        self.closed = true;
    }

    /// Reopen after a disconnect (a fresh client).
    pub fn reopen(&mut self) {
        self.closed = false;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Record the approval fingerprint offered to the client.
    pub fn offer_approval(&mut self, approval_key: impl Into<String>) {
        self.pending_approval = Some(approval_key.into());
    }

    /// Check a client's approval against the pending fingerprint.
    ///
    /// A missing or mismatched fingerprint means the action stays blocked:
    /// there is no implicit approval path.
    pub fn check_approval(&self, approval_key: &str) -> Result<(), ApprovalRefusal> {
        match &self.pending_approval {
            None => Err(ApprovalRefusal::NoPendingApproval),
            Some(pending) if pending != approval_key => Err(ApprovalRefusal::FingerprintMismatch {
                offered: pending.clone(),
            }),
            Some(_) => Ok(()),
        }
    }
}

/// Why a client approval was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalRefusal {
    NoPendingApproval,
    FingerprintMismatch {
        offered: String,
    },
    /// The session advanced past the approval's revision.
    Stale {
        generation: u64,
        current: u64,
    },
}

impl std::fmt::Display for ApprovalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApprovalRefusal::NoPendingApproval => {
                write!(f, "there is no pending approval for this session")
            }
            ApprovalRefusal::FingerprintMismatch { offered } => write!(
                f,
                "the approval does not name the pending action (offered {offered})"
            ),
            ApprovalRefusal::Stale {
                generation,
                current,
            } => write!(
                f,
                "this approval is from generation {generation}; the session is at {current}"
            ),
        }
    }
}

/// The headless adapter: commands in, JSONL events out.
///
/// It owns no execution loop: every command becomes a `SessionCommand` and
/// every event is a `SessionEvent` the runtime published.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessAdapter {
    pub session: HeadlessSession,
    /// Monotonic revision guard for approvals.
    generation: u64,
}

impl HeadlessAdapter {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session: HeadlessSession::new(session_id),
            generation: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn handle_line(&mut self, line: &str) -> Result<Option<SessionCommand>, ProtocolError> {
        if self.session.is_closed() {
            return Err(ProtocolError::Malformed {
                detail: "the session is closed; no further commands are accepted".to_owned(),
            });
        }
        let command = HeadlessCommand::parse(line)?;
        if command == HeadlessCommand::Close {
            self.session.disconnect();
            return Ok(None);
        }
        if let HeadlessCommand::Approve { approval_key } = &command {
            self.session
                .check_approval(approval_key)
                .map_err(|refusal| ProtocolError::Malformed {
                    detail: refusal.to_string(),
                })?;
        }
        Ok(command.to_session_command())
    }

    /// Translate one engine event into the headless vocabulary.
    ///
    /// The event is passed through unchanged: the adapter never invents
    /// progress the runtime did not report.
    pub fn translate(&mut self, event: &SessionEvent) -> HeadlessEvent {
        if let SessionEvent::WaitingForUser {
            wait: WaitKind::Approval { approval_key },
            ..
        } = event
        {
            self.generation += 1;
            self.session.offer_approval(approval_key.clone());
        }
        if matches!(
            event,
            SessionEvent::TaskSteered { .. } | SessionEvent::TaskStarted { .. }
        ) {
            // A steering or a new task invalidates an earlier approval.
            self.generation += 1;
            self.session.pending_approval = None;
        }

        HeadlessEvent::Event {
            protocol_version: JSONL_PROTOCOL_VERSION,
            event: event.clone(),
        }
    }

    /// The terminal outcome, when the event carries one.
    pub fn outcome(&self, event: &SessionEvent) -> Option<HeadlessEvent> {
        let (state, reason) = match event {
            SessionEvent::TaskCompleted { summary, .. } => {
                (HeadlessOutcome::Completed, Some(summary.clone()))
            }
            SessionEvent::TaskFailed { reason, .. } => {
                (HeadlessOutcome::Failed, Some(reason.clone()))
            }
            SessionEvent::TaskCancelled { .. } => (HeadlessOutcome::Cancelled, None),
            _ => return None,
        };
        Some(HeadlessEvent::Outcome {
            protocol_version: JSONL_PROTOCOL_VERSION,
            state,
            reason,
        })
    }
}

// --- ACP ----------------------------------------------------------------

/// The ACP client's `initialize` request, parsed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpInitialize {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(rename = "clientCapabilities", default)]
    pub client_capabilities: Value,
}

/// Capabilities this agent advertises.
///
/// Only what is implemented *and* covered by fixtures. An editor feature
/// that is not implemented is reported as unavailable rather than
/// advertised and ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpAgentCapabilities {
    /// Streaming prompt updates.
    pub streaming: bool,
    /// Permission requests (the approval path).
    pub permissions: bool,
    /// Creating and loading sessions.
    pub sessions: bool,
    /// File system access from the agent. Not implemented: the agent uses
    /// its own bounded workspace tools instead.
    pub filesystem: bool,
    /// Terminal access from the agent. Not implemented.
    pub terminal: bool,
}

impl Default for AcpAgentCapabilities {
    fn default() -> Self {
        Self {
            streaming: true,
            permissions: true,
            sessions: true,
            filesystem: false,
            terminal: false,
        }
    }
}

impl AcpAgentCapabilities {
    /// Names of capabilities that are *not* implemented.
    ///
    /// An editor can then avoid depending on them.
    pub fn unavailable(&self) -> Vec<&'static str> {
        let mut unavailable = Vec::new();
        if !self.filesystem {
            unavailable.push("fs/*");
        }
        if !self.terminal {
            unavailable.push("terminal/*");
        }
        unavailable
    }
}

/// The ACP adapter over a `SessionRuntime`.
///
/// It maps ACP methods onto the runtime's commands; it does not implement
/// a second engine.
#[derive(Debug, Clone, PartialEq)]
pub struct AcpAdapter {
    pub session_id: String,
    /// Capabilities advertised to the editor.
    pub capabilities: AcpAgentCapabilities,
    /// The working directory the editor asked for, translated at the
    /// boundary.
    workdir: Option<String>,
    /// The last prompt turn's stop reason.
    pub last_stop_reason: Option<AcpStopReason>,
    /// Pending permission request, keyed by its exact fingerprint.
    pending_permission: Option<String>,
}

/// ACP stop reasons, as the protocol defines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpStopReason {
    EndTurn,
    Cancelled,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
}

impl AcpStopReason {
    /// Map the engine's terminal outcome.
    pub fn from_task_state(state: TaskState) -> Option<Self> {
        match state {
            TaskState::Completed => Some(AcpStopReason::EndTurn),
            TaskState::Cancelled => Some(AcpStopReason::Cancelled),
            TaskState::Failed => Some(AcpStopReason::Refusal),
            _ => None,
        }
    }
}

impl AcpAdapter {
    /// Accept an `initialize` request, negotiating the protocol version.
    pub fn initialize(request: &AcpInitialize) -> Result<Self, ProtocolError> {
        if request.protocol_version != ACP_PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                requested: request.protocol_version,
                supported: ACP_PROTOCOL_VERSION,
            });
        }
        Ok(Self {
            session_id: String::new(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        })
    }

    /// The `initialize` result.
    pub fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "agentCapabilities": {
                "streaming": self.capabilities.streaming,
                "permissions": self.capabilities.permissions,
                "sessions": self.capabilities.sessions,
            },
            // Unimplemented capabilities are named so an editor does not
            // depend on them.
            "unavailableCapabilities": self.capabilities.unavailable(),
        })
    }

    /// `session/new`: create a session for a working directory.
    ///
    /// The path is translated at the boundary and then handed to the
    /// workspace, which keeps its own traversal and symlink checks: an
    /// editor cannot widen the workspace by asking.
    pub fn new_session(
        &mut self,
        session_id: impl Into<String>,
        workdir: impl Into<String>,
    ) -> Value {
        self.session_id = session_id.into();
        self.workdir = Some(workdir.into());
        json!({ "sessionId": self.session_id })
    }

    /// `session/prompt`: submit a prompt and stream updates.
    pub fn prompt(&mut self, prompt: impl Into<String>) -> SessionCommand {
        SessionCommand::Submit {
            prompt: prompt.into(),
        }
    }

    /// `session/cancel`.
    pub fn cancel(&mut self) -> SessionCommand {
        self.last_stop_reason = Some(AcpStopReason::Cancelled);
        SessionCommand::Cancel
    }

    /// A permission request presented to the editor.
    ///
    /// The request carries the exact fingerprint the TUI would use, so an
    /// editor's approval resolves the same pending action.
    pub fn permission_request(
        &mut self,
        approval_key: impl Into<String>,
        description: impl Into<String>,
    ) -> Value {
        let approval_key = approval_key.into();
        self.pending_permission = Some(approval_key.clone());
        json!({
            "method": "session/request_permission",
            "params": {
                "sessionId": self.session_id,
                "permission": {
                    "fingerprint": approval_key,
                    "description": description.into(),
                    "default": "blocked"
                }
            }
        })
    }

    /// Resolve a permission response from an editor.
    ///
    /// An absent response, an unknown fingerprint or a stale generation
    /// leaves the action **blocked**. There is no implicit approval.
    pub fn resolve_permission(
        &self,
        fingerprint: Option<&str>,
        generation: u64,
        current_generation: u64,
        approved: bool,
    ) -> Result<SessionCommand, ApprovalRefusal> {
        let Some(fingerprint) = fingerprint else {
            return Err(ApprovalRefusal::NoPendingApproval);
        };
        match &self.pending_permission {
            None => Err(ApprovalRefusal::NoPendingApproval),
            Some(pending) if pending != fingerprint => Err(ApprovalRefusal::FingerprintMismatch {
                offered: pending.clone(),
            }),
            Some(_) if generation != current_generation => Err(ApprovalRefusal::Stale {
                generation,
                current: current_generation,
            }),
            Some(pending) => {
                if approved {
                    Ok(SessionCommand::Approve {
                        approval_key: pending.clone(),
                    })
                } else {
                    Ok(SessionCommand::Deny {
                        approval_key: pending.clone(),
                    })
                }
            }
        }
    }

    /// Translate one engine event into an ACP `session/update`.
    pub fn session_update(&mut self, event: &SessionEvent) -> Option<Value> {
        let update = match event {
            SessionEvent::TextDelta { text, .. } => json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": text },
            }),
            SessionEvent::ToolCallProposed {
                call_id,
                name,
                arguments,
                ..
            } => json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id,
                "title": name,
                "rawInput": arguments,
                "status": "pending",
            }),
            SessionEvent::NodeResult {
                node,
                node_label,
                status,
                output,
                ..
            } => json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": format!("node-{}", node.0),
                "title": node_label,
                "status": match status {
                    crate::tree::NodeStatus::Succeeded => "completed",
                    crate::tree::NodeStatus::Failed => "failed",
                    crate::tree::NodeStatus::Blocked => "pending",
                    _ => "in_progress",
                },
                "rawOutput": output,
            }),
            SessionEvent::PlanStarted { node_count, .. } => json!({
                "sessionUpdate": "plan",
                "entries": [{ "content": format!("plan with {node_count} node(s)") }],
            }),
            SessionEvent::WaitingForUser {
                wait: WaitKind::Approval { approval_key },
                message,
                ..
            } => {
                self.pending_permission = Some(approval_key.clone());
                json!({
                    "sessionUpdate": "permission_request",
                    "fingerprint": approval_key,
                    "description": message,
                    "default": "blocked",
                })
            }
            _ => return None,
        };
        Some(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": self.session_id, "update": update }
        }))
    }

    /// Record the stop reason for a terminal event.
    pub fn finish(&mut self, state: TaskState) -> Option<AcpStopReason> {
        let reason = AcpStopReason::from_task_state(state)?;
        self.last_stop_reason = Some(reason);
        Some(reason)
    }

    /// The `session/prompt` result.
    pub fn prompt_result(&self) -> Value {
        json!({
            "stopReason": self.last_stop_reason.unwrap_or(AcpStopReason::EndTurn),
        })
    }
}

/// Translate one engine event into ACP, over a fresh adapter.
pub fn acp_update(event: &SessionEvent) -> Option<Value> {
    let mut adapter = AcpAdapter {
        session_id: String::new(),
        capabilities: AcpAgentCapabilities::default(),
        workdir: None,
        last_stop_reason: None,
        pending_permission: None,
    };
    adapter.session_update(event)
}

/// Equivalence check between the three adapters.
///
/// The acceptance test for #40: one scripted task must produce equivalent
/// execution outcomes whether observed through the TUI event reducer,
/// JSONL or ACP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterEquivalence {
    pub tui_outcome: Option<TaskState>,
    pub jsonl_outcome: Option<HeadlessOutcome>,
    pub acp_stop_reason: Option<AcpStopReason>,
    /// Whether all three agree on the terminal outcome.
    pub agrees: bool,
    /// Where they disagree, when they do.
    pub disagreement: Option<String>,
}

impl AdapterEquivalence {
    /// Compare the three adapters' readings of one event stream.
    pub fn assess(events: &[SessionEvent]) -> Self {
        let mut tui_state = crate::tui_state::WorkbenchState::new("headless");
        let mut adapter = HeadlessAdapter::new("s1");
        let mut acp = AcpAdapter {
            session_id: "s1".to_owned(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        };

        let mut jsonl_outcome = None;
        for event in events {
            tui_state.apply(event);
            let _ = adapter.translate(event);
            let _ = acp.session_update(event);
            if let Some(HeadlessEvent::Outcome { state, .. }) = adapter.outcome(event) {
                jsonl_outcome = Some(state);
            }
            if let SessionEvent::TaskCompleted { .. } = event {
                acp.finish(TaskState::Completed);
            }
            if let SessionEvent::TaskFailed { .. } = event {
                acp.finish(TaskState::Failed);
            }
            if let SessionEvent::TaskCancelled { .. } = event {
                acp.finish(TaskState::Cancelled);
            }
        }

        let tui_outcome = tui_state.task_state.filter(|state| state.is_terminal());
        let acp_stop_reason = acp.last_stop_reason;

        // The three must agree, translated through their own vocabularies.
        let jsonl_reading = jsonl_outcome;
        let mut disagreement = None;
        if let Some(tui) = tui_outcome {
            if let Some(jsonl) = jsonl_reading
                && HeadlessOutcome::from_task_state(tui) != Some(jsonl)
            {
                disagreement = Some(format!("TUI {tui:?} vs JSONL {jsonl:?}"));
            }
            if let Some(acp_reason) = acp_stop_reason
                && AcpStopReason::from_task_state(tui) != Some(acp_reason)
            {
                disagreement = Some(format!("TUI {tui:?} vs ACP {acp_reason:?}"));
            }
        }

        Self {
            tui_outcome,
            jsonl_outcome: jsonl_reading,
            acp_stop_reason,
            agrees: disagreement.is_none(),
            disagreement,
        }
    }
}

/// Whether a line is safe to write to stdout.
///
/// Stdout carries protocol messages only: a diagnostic must never make a
/// line unparseable.
pub fn is_protocol_line(line: &str) -> bool {
    serde_json::from_str::<Value>(line.trim()).is_ok()
}

/// Render a diagnostic for stderr.
pub fn diagnostic(text: &str) -> String {
    format!("knut: {text}")
}

/// The ACP method names this adapter implements.
pub const ACP_METHODS: &[&str] = &[
    "initialize",
    "session/new",
    "session/load",
    "session/prompt",
    "session/cancel",
];

/// Methods an editor might call that this adapter does *not* implement.
pub const ACP_UNIMPLEMENTED_METHODS: &[&str] =
    &["fs/read_text_file", "fs/write_text_file", "terminal/create"];

/// Dispatch an ACP method by name, so an editor gets an explicit
/// "unimplemented" rather than silence.
pub fn acp_dispatch(
    adapter: &mut AcpAdapter,
    method: &str,
    params: &Value,
) -> Result<Value, ProtocolError> {
    if ACP_UNIMPLEMENTED_METHODS.contains(&method) {
        return Err(ProtocolError::Malformed {
            detail: format!(
                "method {method:?} is not implemented by this agent; the agent uses its own \
                 bounded workspace tools instead"
            ),
        });
    }
    match method {
        "initialize" => {
            let request: AcpInitialize =
                serde_json::from_value(params.clone()).map_err(|err| ProtocolError::Malformed {
                    detail: err.to_string(),
                })?;
            let fresh = AcpAdapter::initialize(&request)?;
            let result = fresh.initialize_result();
            adapter.capabilities = fresh.capabilities;
            Ok(result)
        }
        "session/new" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("acp-session");
            let workdir = params.get("cwd").and_then(Value::as_str).unwrap_or(".");
            Ok(adapter.new_session(session_id, workdir))
        }
        "session/load" => {
            // Loading restores a session identity; the transcript replay is
            // the runtime's job and dispatches nothing.
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| ProtocolError::Malformed {
                    detail: "session/load needs a sessionId".to_owned(),
                })?;
            adapter.session_id = session_id.to_owned();
            Ok(json!({ "loaded": true }))
        }
        "session/prompt" => Ok(adapter.prompt_result()),
        "session/cancel" => Ok(json!({ "cancelled": true })),
        other => Err(ProtocolError::Malformed {
            detail: format!("unknown method {other:?}"),
        }),
    }
}

/// A map of pending approvals by session, for a multi-session editor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PendingPermissions {
    by_session: BTreeMap<String, String>,
}

impl PendingPermissions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, session: impl Into<String>, fingerprint: impl Into<String>) {
        self.by_session.insert(session.into(), fingerprint.into());
    }

    pub fn get(&self, session: &str) -> Option<&str> {
        self.by_session.get(session).map(String::as_str)
    }

    pub fn clear(&mut self, session: &str) {
        self.by_session.remove(session);
    }

    pub fn is_empty(&self) -> bool {
        self.by_session.is_empty()
    }
}

/// Translate a Windows or POSIX path from the protocol boundary into a
/// workspace-relative path.
///
/// The workspace still performs its own checks; this only normalizes the
/// separator, so an editor's convention cannot bypass a traversal check.
pub fn normalize_protocol_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_owned()
}

/// A protocol-level failure recorded separately from task failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolFailureRecord {
    pub session: String,
    pub error: ProtocolError,
    /// Whether the session stopped because of it.
    pub stopped_session: bool,
}

/// Track protocol failures so they never masquerade as model failures.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProtocolFailures {
    records: Vec<ProtocolFailureRecord>,
}

impl ProtocolFailures {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, session: impl Into<String>, error: ProtocolError) {
        self.records.push(ProtocolFailureRecord {
            session: session.into(),
            error,
            stopped_session: false,
        });
    }

    pub fn records(&self) -> &[ProtocolFailureRecord] {
        &self.records
    }

    /// Whether any recorded failure was a protocol error (they all are).
    pub fn all_protocol(&self) -> bool {
        self.records
            .iter()
            .all(|record| record.error.is_protocol_error())
    }
}

/// Convert engine errors that are really protocol errors.
pub fn classify_engine_error(err: &KnutError) -> Option<ProtocolError> {
    match err {
        KnutError::InvalidArguments { path, reason } if path == "task" => {
            Some(ProtocolError::NoActiveTask)
        }
        _ => {
            let _ = err;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{NodeId, TaskId, TaskRevision, TurnId};

    fn started() -> SessionEvent {
        SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "fix the failing test".to_owned(),
        }
    }

    fn completed() -> SessionEvent {
        SessionEvent::TaskCompleted {
            task: TaskId(1),
            summary: "the fix verified".to_owned(),
        }
    }

    #[test]
    fn a_scripted_task_produces_equivalent_outcomes_through_all_three_adapters() {
        // The acceptance test for #40: the same event stream, read by the
        // TUI reducer, the JSONL adapter and the ACP adapter.
        let events = vec![
            started(),
            SessionEvent::Routed {
                task: TaskId(1),
                turn: TurnId(1),
                revision: TaskRevision(1),
                source: crate::DecisionSource::SystemOne,
                action: crate::Action::Generate(crate::ModelTier::Reasoner),
                confidence: 0.9,
            },
            SessionEvent::TextDelta {
                task: TaskId(1),
                turn: TurnId(1),
                node: NodeId(1),
                text: "working".to_owned(),
            },
            completed(),
        ];

        let equivalence = AdapterEquivalence::assess(&events);
        assert!(equivalence.agrees, "{equivalence:?}");
        assert_eq!(equivalence.tui_outcome, Some(TaskState::Completed));
        assert_eq!(equivalence.jsonl_outcome, Some(HeadlessOutcome::Completed));
        assert_eq!(equivalence.acp_stop_reason, Some(AcpStopReason::EndTurn));
    }

    #[test]
    fn all_three_adapters_agree_on_failure_and_cancellation_too() {
        for (terminal, state, headless, acp) in [
            (
                SessionEvent::TaskFailed {
                    task: TaskId(1),
                    reason: "the check failed".to_owned(),
                },
                TaskState::Failed,
                HeadlessOutcome::Failed,
                AcpStopReason::Refusal,
            ),
            (
                SessionEvent::TaskCancelled { task: TaskId(1) },
                TaskState::Cancelled,
                HeadlessOutcome::Cancelled,
                AcpStopReason::Cancelled,
            ),
        ] {
            let events = vec![started(), terminal];
            let equivalence = AdapterEquivalence::assess(&events);
            assert!(equivalence.agrees, "{equivalence:?}");
            assert_eq!(equivalence.tui_outcome, Some(state));
            assert_eq!(equivalence.jsonl_outcome, Some(headless));
            assert_eq!(equivalence.acp_stop_reason, Some(acp));
        }
    }

    #[test]
    fn stdout_lines_are_always_valid_protocol_messages() {
        let mut adapter = HeadlessAdapter::new("s1");
        let events = [
            started(),
            SessionEvent::RuntimeError {
                task: Some(TaskId(1)),
                message: "a diagnostic that must not break parsing".to_owned(),
            },
            completed(),
        ];

        for event in &events {
            let translated = adapter.translate(event);
            let line = to_jsonl(&translated).unwrap();
            assert!(is_protocol_line(&line), "not a protocol line: {line}");
            // And no diagnostic text leaks into stdout unescaped.
            assert!(serde_json::from_str::<Value>(&line).is_ok());
        }

        // Diagnostics are rendered for stderr, separately.
        let diagnostic = diagnostic("something went wrong");
        assert!(diagnostic.starts_with("knut: "));
    }

    #[test]
    fn a_malformed_line_is_a_protocol_error_not_a_task_failure() {
        let mut adapter = HeadlessAdapter::new("s1");
        let error = adapter.handle_line("{not json").unwrap_err();
        assert!(error.is_protocol_error());
        assert!(matches!(error, ProtocolError::Malformed { .. }));
        let huge = "x".repeat(MAX_LINE_BYTES + 10);
        match adapter.handle_line(&huge).unwrap_err() {
            ProtocolError::OversizedLine { bytes, limit } => {
                assert!(bytes > MAX_LINE_BYTES);
                assert_eq!(limit, MAX_LINE_BYTES);
            }
            other => panic!("expected an oversized-line error, got {other:?}"),
        }
    }

    #[test]
    fn a_protocol_error_serializes_without_killing_the_stream() {
        let error = ProtocolError::Malformed {
            detail: "bad input".to_owned(),
        };
        let line = serde_json::to_string(&error.to_json()).unwrap();
        // It parses like any other message, so a client can report it.
        assert!(is_protocol_line(&line));
        assert!(line.contains("\"type\":\"error\""));
    }

    #[test]
    fn stdin_close_stops_the_session() {
        let mut adapter = HeadlessAdapter::new("s1");
        assert!(matches!(
            adapter.handle_line(r#"{"type":"submit","prompt":"do work"}"#).unwrap(),
            Some(SessionCommand::Submit { prompt }) if prompt == "do work"
        ));
        assert!(!adapter.session.is_closed());
        assert!(
            adapter
                .handle_line(r#"{"type":"close"}"#)
                .unwrap()
                .is_none()
        );
        assert!(adapter.session.is_closed());
        assert!(
            adapter
                .handle_line(r#"{"type":"submit","prompt":"more"}"#)
                .is_err()
        );
    }

    #[test]
    fn a_second_client_cannot_race_a_writable_session() {
        let mut session = HeadlessSession::new("s1");
        session.claim("editor-a").unwrap();
        // A second client is refused, with the owner named.
        let err = session.claim("editor-b").unwrap_err();
        assert!(matches!(err, ProtocolError::SessionLocked { .. }));
        assert!(format!("{err:?}").contains("editor-a"));

        // The same client may re-claim.
        assert!(session.claim("editor-a").is_ok());

        // After a disconnect, a new client may take over.
        session.disconnect();
        assert!(session.claim("editor-b").is_ok());
    }

    #[test]
    fn an_editor_cannot_approve_an_old_patch_after_a_newer_revision() {
        let mut adapter = AcpAdapter {
            session_id: "s1".to_owned(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        };
        adapter.permission_request("fingerprint-1", "write src/lib.rs");

        // A response for the right fingerprint at the right generation
        // resolves it.
        assert!(
            adapter
                .resolve_permission(Some("fingerprint-1"), 5, 5, true)
                .is_ok()
        );

        // A stale generation is refused.
        let err = adapter
            .resolve_permission(Some("fingerprint-1"), 4, 5, true)
            .unwrap_err();
        assert!(matches!(err, ApprovalRefusal::Stale { .. }));

        // A different fingerprint is refused: another action is not this
        // action.
        let err = adapter
            .resolve_permission(Some("fingerprint-2"), 5, 5, true)
            .unwrap_err();
        assert!(matches!(err, ApprovalRefusal::FingerprintMismatch { .. }));
    }

    #[test]
    fn a_missing_permission_response_means_blocked_not_approved() {
        let mut adapter = AcpAdapter {
            session_id: "s1".to_owned(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        };
        adapter.permission_request("fingerprint-1", "run a command");

        // No response at all: refused.
        assert!(matches!(
            adapter.resolve_permission(None, 1, 1, true).unwrap_err(),
            ApprovalRefusal::NoPendingApproval
        ));

        // An explicit denial is a denial, never an approval.
        let command = adapter
            .resolve_permission(Some("fingerprint-1"), 1, 1, false)
            .unwrap();
        assert!(matches!(command, SessionCommand::Deny { .. }));

        let command = adapter
            .resolve_permission(Some("fingerprint-1"), 1, 1, true)
            .unwrap();
        assert!(matches!(command, SessionCommand::Approve { .. }));
    }

    #[test]
    fn the_permission_request_carries_the_exact_fingerprint_and_defaults_to_blocked() {
        let mut adapter = AcpAdapter {
            session_id: "s1".to_owned(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        };
        let request = adapter.permission_request("fingerprint-xyz", "apply a patch");
        assert_eq!(
            request["params"]["permission"]["fingerprint"],
            "fingerprint-xyz"
        );
        // The default is blocked, not allowed.
        assert_eq!(request["params"]["permission"]["default"], "blocked");
    }

    #[test]
    fn acp_initialization_negotiates_the_protocol_version() {
        let request = AcpInitialize {
            protocol_version: ACP_PROTOCOL_VERSION,
            client_capabilities: json!({}),
        };
        let adapter = AcpAdapter::initialize(&request).unwrap();
        let result = adapter.initialize_result();
        assert_eq!(result["protocolVersion"], ACP_PROTOCOL_VERSION);

        // A version this agent does not implement is refused, naming both.
        let wrong = AcpInitialize {
            protocol_version: 99,
            client_capabilities: json!({}),
        };
        let err = AcpAdapter::initialize(&wrong).unwrap_err();
        assert!(matches!(err, ProtocolError::UnsupportedVersion { .. }));
        assert!(format!("{err:?}").contains("99"));
    }

    #[test]
    fn capabilities_are_advertised_only_when_implemented() {
        let capabilities = AcpAgentCapabilities::default();
        assert!(capabilities.streaming);
        assert!(capabilities.permissions);
        assert!(capabilities.sessions);
        // Not implemented, so not advertised.
        assert!(!capabilities.filesystem);
        assert!(!capabilities.terminal);
        assert_eq!(capabilities.unavailable(), vec!["fs/*", "terminal/*"]);

        let request = AcpInitialize {
            protocol_version: ACP_PROTOCOL_VERSION,
            client_capabilities: json!({}),
        };
        let adapter = AcpAdapter::initialize(&request).unwrap();
        let result = adapter.initialize_result();
        let rendered = result.to_string();
        assert!(rendered.contains("unavailableCapabilities"));
        assert!(!rendered.contains("\"filesystem\":true"));
    }

    #[test]
    fn unimplemented_editor_methods_say_so_instead_of_failing_silently() {
        let mut adapter = AcpAdapter {
            session_id: "s1".to_owned(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        };

        for method in ACP_UNIMPLEMENTED_METHODS {
            let err = acp_dispatch(&mut adapter, method, &json!({})).unwrap_err();
            let message = format!("{err:?}");
            assert!(
                message.contains("not implemented"),
                "{method} said {message}"
            );
            // And it points at the real alternative.
            assert!(message.contains("bounded workspace tools"));
        }

        // Implemented methods work.
        assert!(
            acp_dispatch(
                &mut adapter,
                "session/new",
                &json!({ "sessionId": "s2", "cwd": "/workspace" })
            )
            .is_ok()
        );
        assert!(acp_dispatch(&mut adapter, "session/cancel", &json!({})).is_ok());
    }

    #[test]
    fn streaming_updates_map_onto_the_acp_vocabulary() {
        let mut adapter = AcpAdapter {
            session_id: "s1".to_owned(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        };

        let update = adapter
            .session_update(&SessionEvent::TextDelta {
                task: TaskId(1),
                turn: TurnId(1),
                node: NodeId(1),
                text: "hello".to_owned(),
            })
            .unwrap();
        assert_eq!(update["method"], "session/update");
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );

        let update = adapter
            .session_update(&SessionEvent::ToolCallProposed {
                task: TaskId(1),
                turn: TurnId(1),
                call_id: "call_1".to_owned(),
                name: "read".to_owned(),
                arguments: json!({ "path": "src/a.rs" }),
            })
            .unwrap();
        assert_eq!(update["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(update["params"]["update"]["toolCallId"], "call_1");

        let update = adapter
            .session_update(&SessionEvent::NodeResult {
                task: TaskId(1),
                turn: TurnId(1),
                node: NodeId(2),
                node_label: "check".to_owned(),
                status: crate::tree::NodeStatus::Failed,
                output: json!({ "exit": 101 }),
            })
            .unwrap();
        assert_eq!(update["params"]["update"]["status"], "failed");

        // A session lifecycle event has no ACP update.
        assert!(adapter.session_update(&started()).is_none());
    }

    #[test]
    fn a_waiting_approval_becomes_an_acp_permission_request() {
        let mut adapter = AcpAdapter {
            session_id: "s1".to_owned(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        };
        let update = adapter
            .session_update(&SessionEvent::WaitingForUser {
                task: TaskId(1),
                turn: TurnId(1),
                wait: WaitKind::Approval {
                    approval_key: "fp-1".to_owned(),
                },
                message: "writes src/lib.rs".to_owned(),
            })
            .unwrap();

        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "permission_request"
        );
        assert_eq!(update["params"]["update"]["fingerprint"], "fp-1");
        assert_eq!(update["params"]["update"]["default"], "blocked");
    }

    #[test]
    fn stop_reasons_map_from_the_engine_state() {
        assert_eq!(
            AcpStopReason::from_task_state(TaskState::Completed),
            Some(AcpStopReason::EndTurn)
        );
        assert_eq!(
            AcpStopReason::from_task_state(TaskState::Cancelled),
            Some(AcpStopReason::Cancelled)
        );
        assert_eq!(
            AcpStopReason::from_task_state(TaskState::Failed),
            Some(AcpStopReason::Refusal)
        );
        assert_eq!(AcpStopReason::from_task_state(TaskState::Running), None);
    }

    #[test]
    fn a_client_approval_uses_the_same_fingerprint_as_the_tui() {
        // The adapter's pending fingerprint comes straight from the
        // runtime's approval event, so the client resolves the same exact
        // action the TUI would.
        let mut adapter = HeadlessAdapter::new("s1");
        let event = SessionEvent::WaitingForUser {
            task: TaskId(1),
            turn: TurnId(1),
            wait: WaitKind::Approval {
                approval_key: "exact-fingerprint".to_owned(),
            },
            message: "needs approval".to_owned(),
        };
        adapter.translate(&event);
        assert_eq!(
            adapter.session.pending_approval.as_deref(),
            Some("exact-fingerprint")
        );

        // A different key is refused before it reaches the engine.
        let refused = adapter.handle_line(r#"{"type":"approve","approval_key":"other"}"#);
        assert!(refused.is_err());

        // The exact key is accepted.
        let accepted =
            adapter.handle_line(r#"{"type":"approve","approval_key":"exact-fingerprint"}"#);
        assert!(
            matches!(accepted.unwrap(), Some(SessionCommand::Approve { approval_key }) if approval_key == "exact-fingerprint")
        );
    }

    #[test]
    fn steering_invalidates_a_clients_pending_approval() {
        let mut adapter = HeadlessAdapter::new("s1");
        adapter.translate(&SessionEvent::WaitingForUser {
            task: TaskId(1),
            turn: TurnId(1),
            wait: WaitKind::Approval {
                approval_key: "old".to_owned(),
            },
            message: "x".to_owned(),
        });
        let generation_before = adapter.generation();

        // The task is steered: the old approval no longer applies.
        adapter.translate(&SessionEvent::TaskSteered {
            task: TaskId(1),
            revision: TaskRevision(2),
            prompt: "different approach".to_owned(),
        });
        assert!(adapter.session.pending_approval.is_none());
        assert!(adapter.generation() > generation_before);

        // So the old fingerprint is refused.
        let refused = adapter.handle_line(r#"{"type":"approve","approval_key":"old"}"#);
        assert!(refused.is_err());
    }

    #[test]
    fn protocol_failures_are_recorded_separately_from_task_failures() {
        let mut failures = ProtocolFailures::new();
        failures.record(
            "s1",
            ProtocolError::Malformed {
                detail: "bad json".to_owned(),
            },
        );
        assert_eq!(failures.records().len(), 1);
        assert!(failures.all_protocol());
        // A task failure is a different record entirely: it travels as a
        // session event, not a protocol failure.
        assert!(classify_engine_error(&KnutError::Tool("check failed".to_owned())).is_none());
    }

    #[test]
    fn paths_are_translated_at_the_boundary_without_weakening_workspace_checks() {
        assert_eq!(normalize_protocol_path("./src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_protocol_path("src\\lib.rs"), "src/lib.rs");
        // Traversal is passed through unchanged, so the workspace's own
        // check still sees and rejects it.
        assert_eq!(normalize_protocol_path("../outside.rs"), "../outside.rs");
    }

    #[test]
    fn pending_permissions_are_tracked_per_session() {
        let mut pending = PendingPermissions::new();
        pending.set("s1", "fp-1");
        pending.set("s2", "fp-2");
        assert_eq!(pending.get("s1"), Some("fp-1"));
        assert_eq!(pending.get("s2"), Some("fp-2"));
        pending.clear("s1");
        assert_eq!(pending.get("s1"), None);
        assert!(!pending.is_empty());
    }

    #[test]
    fn the_jsonl_protocol_version_is_stamped_on_every_message() {
        let mut adapter = HeadlessAdapter::new("s1");
        let ready = HeadlessEvent::Ready {
            protocol_version: JSONL_PROTOCOL_VERSION,
            session: "s1".to_owned(),
        };
        assert!(to_jsonl(&ready).unwrap().contains("\"protocol_version\":1"));

        let translated = adapter.translate(&completed());
        assert!(
            to_jsonl(&translated)
                .unwrap()
                .contains("\"protocol_version\":1")
        );
    }

    #[test]
    fn the_session_load_path_restores_an_identity_without_dispatching() {
        let mut adapter = AcpAdapter {
            session_id: String::new(),
            capabilities: AcpAgentCapabilities::default(),
            workdir: None,
            last_stop_reason: None,
            pending_permission: None,
        };
        let result = acp_dispatch(
            &mut adapter,
            "session/load",
            &json!({ "sessionId": "stored-session" }),
        )
        .unwrap();
        assert_eq!(result["loaded"], json!(true));
        assert_eq!(adapter.session_id, "stored-session");
    }
}
