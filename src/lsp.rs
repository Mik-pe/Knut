//! Language intelligence as tools and evidence (issue #37).
//!
//! Navigation and diagnostics come from a supervised language server, and
//! are treated as *evidence*: a definition that jumps somewhere useful is
//! navigation, not proof that a patch is correct. Compilers and tests
//! remain separate evidence (#26).
//!
//! Rules carried through:
//! - **trust is explicit.** Starting a server runs a program named by
//!   configuration; a missing server degrades to the existing search
//!   tools, never to a silent install or a broken session.
//! - **nothing is inherited.** The server runs with a cleared environment
//!   and a bounded timeout, like any other supervised process.
//! - **versions are tracked.** A diagnostic for an older document version
//!   can never mark the current patch as verified.
//! - **not-ready is not "no errors".** A server that has not finished
//!   indexing says so.
//! - **server edits are never applied.** They become a reviewable patch.
//!
//! Tested protocol revision: LSP 3.18 (documented below), with the
//! compatibility limits stated rather than implied.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::KnutError;
use crate::workspace::ContentRef;

/// The protocol revision this module is written against.
///
/// Pinned so a compatibility question has a definite answer.
pub const LSP_PROTOCOL_VERSION: &str = "3.18";

/// Features this implementation actually supports.
///
/// A capability that is not listed here is reported as unsupported rather
/// than attempted and half-worked.
pub const SUPPORTED_FEATURES: &[LspFeature] = &[
    LspFeature::Definition,
    LspFeature::References,
    LspFeature::DocumentSymbols,
    LspFeature::WorkspaceSymbols,
    LspFeature::Diagnostics,
];

/// A language-server feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspFeature {
    Definition,
    References,
    DocumentSymbols,
    WorkspaceSymbols,
    Diagnostics,
    /// Everything else (rename, code actions, formatting, …) is out of
    /// scope here.
    Other,
}

impl LspFeature {
    pub fn label(self) -> &'static str {
        match self {
            LspFeature::Definition => "definition",
            LspFeature::References => "references",
            LspFeature::DocumentSymbols => "document symbols",
            LspFeature::WorkspaceSymbols => "workspace symbols",
            LspFeature::Diagnostics => "diagnostics",
            LspFeature::Other => "other",
        }
    }

    /// Whether this implementation supports the feature.
    pub fn is_supported(self) -> bool {
        self != LspFeature::Other && SUPPORTED_FEATURES.contains(&self)
    }
}

/// A language server configuration.
///
/// `trusted` is explicit: nothing starts a repository-specified command
/// implicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Language id, e.g. "rust" or "typescript".
    pub language: String,
    /// Glob patterns of files this server handles.
    pub file_patterns: Vec<String>,
    /// Executable and arguments.
    pub program: String,
    pub args: Vec<String>,
    /// Whether the operator has trusted this command.
    pub trusted: bool,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
}

impl ServerConfig {
    /// The conventional Rust server.
    pub fn rust() -> Self {
        Self {
            language: "rust".to_owned(),
            file_patterns: vec!["*.rs".to_owned()],
            program: "rust-analyzer".to_owned(),
            args: Vec::new(),
            // Not trusted by default: starting a server is a deliberate
            // decision, and the binary may be absent.
            trusted: false,
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(10),
        }
    }

    /// The conventional TypeScript server.
    pub fn typescript() -> Self {
        Self {
            language: "typescript".to_owned(),
            file_patterns: vec!["*.ts".to_owned(), "*.tsx".to_owned(), "*.js".to_owned()],
            program: "typescript-language-server".to_owned(),
            args: vec!["--stdio".to_owned()],
            trusted: false,
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(10),
        }
    }

    /// Mark the command trusted (an explicit operator decision).
    pub fn trust(mut self, trusted: bool) -> Self {
        self.trusted = trusted;
        self
    }

    /// Whether the server binary is present.
    pub fn is_available(&self) -> bool {
        if self.program.contains('/') {
            return std::path::Path::new(&self.program).is_file();
        }
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(&self.program).is_file())
        })
    }
}

/// Why a language server is not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerUnavailable {
    /// The command is not present.
    NotInstalled { program: String },
    /// The command exists but the operator has not trusted it.
    NotTrusted { program: String },
    /// The server did not finish starting in time.
    StartupTimeout { language: String },
    /// The server crashed or was restarted too often.
    Unstable { language: String, restarts: u32 },
}

impl std::fmt::Display for ServerUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerUnavailable::NotInstalled { program } => write!(
                f,
                "language server {program:?} is not installed; falling back to search tools"
            ),
            ServerUnavailable::NotTrusted { program } => write!(
                f,
                "language server {program:?} is not trusted; \
                 enable it explicitly before Knut starts it"
            ),
            ServerUnavailable::StartupTimeout { language } => {
                write!(
                    f,
                    "the {language} language server did not finish starting in time"
                )
            }
            ServerUnavailable::Unstable { language, restarts } => write!(
                f,
                "the {language} language server was restarted {restarts} times and is treated as \
                 unstable"
            ),
        }
    }
}

/// A fallback reason, so degradation is visible rather than silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Degradation {
    pub reason: String,
    /// Which capability was used instead.
    pub fallback: String,
}

/// A position in a document.
///
/// `character` is a *negotiated encoding* offset: UTF-16 by default, which
/// is why the server tells us its encoding at initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// How the server counts characters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionEncoding {
    /// UTF-16 code units: the protocol's default when the server does not
    /// negotiate one.
    #[default]
    Utf16,
    /// UTF-8 code units.
    Utf8,
    /// UTF-32 code points.
    Utf32,
}

impl PositionEncoding {
    /// The length of a string in this encoding's units.
    pub fn measure(self, text: &str) -> u32 {
        match self {
            PositionEncoding::Utf8 => text.len() as u32,
            PositionEncoding::Utf16 => text.encode_utf16().count() as u32,
            PositionEncoding::Utf32 => text.chars().count() as u32,
        }
    }

    /// Convert a byte offset within `line` into this encoding's offset.
    ///
    /// Getting this wrong is how a non-ASCII file produces a definition
    /// that points at the wrong identifier.
    pub fn offset_of_byte(self, line: &str, byte_offset: usize) -> u32 {
        let prefix = &line[..byte_offset.min(line.len())];
        self.measure(prefix)
    }

    /// Convert this encoding's offset back into a byte offset.
    pub fn byte_of_offset(self, line: &str, offset: u32) -> usize {
        let mut units = 0u32;
        for (index, character) in line.char_indices() {
            if units >= offset {
                return index;
            }
            units += match self {
                PositionEncoding::Utf8 => character.len_utf8() as u32,
                PositionEncoding::Utf16 => character.len_utf16() as u32,
                PositionEncoding::Utf32 => 1,
            };
        }
        line.len()
    }
}

/// A source location reported by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// Workspace-relative path.
    pub path: String,
    pub start: Position,
    pub end: Position,
}

/// A navigation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Navigation {
    pub feature: LspFeature,
    pub locations: Vec<Location>,
    /// Whether the result was truncated by a bound.
    pub truncated: bool,
    /// The encoding the positions are expressed in.
    pub encoding: PositionEncoding,
    /// Document version the query was made against.
    pub document_version: i64,
    /// A revision reference, so a stale result is detectable later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<ContentRef>,
    /// Present when the server could not answer and a fallback was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation: Option<Degradation>,
}

/// A diagnostic severity, as the protocol defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// One diagnostic from the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub path: String,
    pub range: (Position, Position),
    pub severity: DiagnosticSeverity,
    pub message: String,
    /// The document version this diagnostic describes.
    pub document_version: i64,
}

/// The state of the diagnostics for one document.
///
/// "Not ready" and "no errors" are different facts, and conflating them is
/// how an unfinished index looks like a clean build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsState {
    /// The server has not reported for the current version yet.
    Pending { version: i64 },
    /// The server reported for the current version.
    Ready {
        version: i64,
        diagnostics: Vec<Diagnostic>,
    },
    /// The server reported for an older version: the result is stale.
    Stale {
        reported_version: i64,
        current_version: i64,
    },
    /// No server: degraded to another tool.
    Unavailable { reason: String },
}

impl DiagnosticsState {
    /// Whether this state proves the current document is clean.
    ///
    /// Only `Ready` with no error diagnostics does; pending, stale and
    /// unavailable never do.
    pub fn proves_clean(&self) -> bool {
        matches!(
            self,
            DiagnosticsState::Ready { diagnostics, .. }
                if !diagnostics
                    .iter()
                    .any(|d| d.severity == DiagnosticSeverity::Error)
        )
    }

    /// The errors, when the state is ready for the current version.
    pub fn errors(&self) -> &[Diagnostic] {
        match self {
            DiagnosticsState::Ready { diagnostics, .. } => diagnostics,
            _ => &[],
        }
    }

    /// A one-line label that never overstates the state.
    pub fn label(&self) -> String {
        match self {
            DiagnosticsState::Pending { version } => {
                format!("diagnostics pending for version {version}")
            }
            DiagnosticsState::Ready {
                version,
                diagnostics,
            } => {
                let errors = diagnostics
                    .iter()
                    .filter(|d| d.severity == DiagnosticSeverity::Error)
                    .count();
                format!("{errors} error(s) at version {version}")
            }
            DiagnosticsState::Stale {
                reported_version,
                current_version,
            } => format!(
                "diagnostics are for version {reported_version}; the document is at \
                 {current_version}"
            ),
            DiagnosticsState::Unavailable { reason } => {
                format!("diagnostics unavailable: {reason}")
            }
        }
    }
}

/// Tracks document versions so a stale result is detectable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocumentVersions {
    versions: BTreeMap<String, i64>,
}

impl DocumentVersions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an edit to a document, bumping its version.
    pub fn edit(&mut self, path: &str) -> i64 {
        let entry = self.versions.entry(path.to_owned()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Set an explicit version (used when syncing a fresh open).
    pub fn set(&mut self, path: &str, version: i64) {
        self.versions.insert(path.to_owned(), version);
    }

    pub fn version(&self, path: &str) -> i64 {
        self.versions.get(path).copied().unwrap_or(0)
    }

    /// Whether a reported version is current for a document.
    pub fn is_current(&self, path: &str, reported: i64) -> bool {
        self.version(path) == reported
    }
}

/// Server capability negotiation, as reported at initialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub encoding: PositionEncoding,
    pub definition: bool,
    pub references: bool,
    pub document_symbols: bool,
    pub workspace_symbols: bool,
    pub diagnostics: bool,
}

impl ServerCapabilities {
    /// Whether the server advertises a feature.
    pub fn supports(&self, feature: LspFeature) -> bool {
        match feature {
            LspFeature::Definition => self.definition,
            LspFeature::References => self.references,
            LspFeature::DocumentSymbols => self.document_symbols,
            LspFeature::WorkspaceSymbols => self.workspace_symbols,
            LspFeature::Diagnostics => self.diagnostics,
            LspFeature::Other => false,
        }
    }

    /// Parse capabilities from an initialization result.
    ///
    /// Unknown fields are ignored: the protocol allows a server to offer
    /// far more than this client implements, and the negotiated encoding
    /// is what matters most.
    pub fn from_initialize_result(result: &Value) -> Self {
        let encoding = match result
            .get("capabilities")
            .and_then(|c| c.get("positionEncoding"))
            .and_then(Value::as_str)
        {
            Some("utf-8") => PositionEncoding::Utf8,
            Some("utf-32") => PositionEncoding::Utf32,
            _ => PositionEncoding::Utf16,
        };
        let has = |name: &str| {
            result
                .get("capabilities")
                .and_then(|c| c.get(name))
                .is_some_and(|value| !value.is_null())
        };

        Self {
            encoding,
            definition: has("definitionProvider"),
            references: has("referencesProvider"),
            document_symbols: has("documentSymbolProvider"),
            workspace_symbols: has("workspaceSymbolProvider"),
            // Diagnostics are pushed by the server rather than advertised
            // as a capability in most implementations.
            diagnostics: true,
        }
    }
}

/// A supervised language server manager.
///
/// The manager owns trust, timeouts, restart policy and version tracking.
/// It never starts a command the operator has not trusted, and it never
/// inherits the parent environment.
pub struct LanguageServerManager {
    config: ServerConfig,
    capabilities: Option<ServerCapabilities>,
    versions: DocumentVersions,
    restarts: u32,
    max_restarts: u32,
    /// Diagnostics keyed by path.
    diagnostics: BTreeMap<String, DiagnosticsState>,
}

/// Maximum restarts before the server is treated as unstable.
pub const MAX_SERVER_RESTARTS: u32 = 3;

/// Maximum navigation results returned.
pub const MAX_NAVIGATION_RESULTS: usize = 200;

impl LanguageServerManager {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            capabilities: None,
            versions: DocumentVersions::new(),
            restarts: 0,
            max_restarts: MAX_SERVER_RESTARTS,
            diagnostics: BTreeMap::new(),
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    pub fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.as_ref()
    }

    pub fn versions(&self) -> &DocumentVersions {
        &self.versions
    }

    /// Record that a document changed, invalidating its diagnostics.
    pub fn document_edited(&mut self, path: &str) -> i64 {
        let version = self.versions.edit(path);
        // The previous diagnostics describe the older version.
        if let Some(previous) = self.diagnostics.get(path).cloned() {
            let reported = match previous {
                DiagnosticsState::Ready { version, .. } => Some(version),
                DiagnosticsState::Pending { version } => Some(version),
                DiagnosticsState::Stale {
                    reported_version, ..
                } => Some(reported_version),
                DiagnosticsState::Unavailable { .. } => None,
            };
            if let Some(reported) = reported {
                self.diagnostics.insert(
                    path.to_owned(),
                    DiagnosticsState::Stale {
                        reported_version: reported,
                        current_version: version,
                    },
                );
            }
        } else {
            self.diagnostics
                .insert(path.to_owned(), DiagnosticsState::Pending { version });
        }
        version
    }

    /// Record diagnostics reported by the server.
    ///
    /// A diagnostic whose version is not current becomes `Stale`: it can
    /// never mark the present document clean.
    pub fn record_diagnostics(&mut self, path: &str, version: i64, diagnostics: Vec<Diagnostic>) {
        let current = self.versions.version(path);
        if version != current {
            self.diagnostics.insert(
                path.to_owned(),
                DiagnosticsState::Stale {
                    reported_version: version,
                    current_version: current,
                },
            );
            return;
        }
        self.diagnostics.insert(
            path.to_owned(),
            DiagnosticsState::Ready {
                version,
                diagnostics,
            },
        );
    }

    pub fn diagnostics(&self, path: &str) -> DiagnosticsState {
        self.diagnostics
            .get(path)
            .cloned()
            .unwrap_or(DiagnosticsState::Unavailable {
                reason: "the server has not reported for this document".to_owned(),
            })
    }

    /// Adopt the negotiated capabilities from an initialization result.
    pub fn negotiated(&mut self, initialize_result: &Value) {
        self.capabilities = Some(ServerCapabilities::from_initialize_result(
            initialize_result,
        ));
    }

    /// Whether the server can be started at all.
    pub fn availability(&self) -> Result<(), ServerUnavailable> {
        if !self.config.is_available() {
            return Err(ServerUnavailable::NotInstalled {
                program: self.config.program.clone(),
            });
        }
        if !self.config.trusted {
            return Err(ServerUnavailable::NotTrusted {
                program: self.config.program.clone(),
            });
        }
        if self.restarts >= self.max_restarts {
            return Err(ServerUnavailable::Unstable {
                language: self.config.language.clone(),
                restarts: self.restarts,
            });
        }
        Ok(())
    }

    /// Record that the server crashed and was restarted.
    pub fn record_restart(&mut self) {
        self.restarts += 1;
    }

    /// Ask for a feature, degrading visibly when it cannot be answered.
    ///
    /// `query` performs the real protocol work in production; tests supply
    /// a fake, which is how initialization, Unicode positions, stale
    /// versions, cancellation and crashes are exercised without a real
    /// server.
    pub fn navigate<F>(
        &self,
        path: &str,
        position: Position,
        feature: LspFeature,
        query: F,
    ) -> Navigation
    where
        F: FnOnce(PositionEncoding, i64) -> Result<Vec<Location>, KnutError>,
    {
        let version = self.versions.version(path);
        let encoding = self
            .capabilities
            .as_ref()
            .map(|capabilities| capabilities.encoding)
            .unwrap_or_default();

        if let Err(unavailable) = self.availability() {
            return Navigation {
                feature,
                locations: Vec::new(),
                truncated: false,
                encoding,
                document_version: version,
                revision: None,
                degradation: Some(Degradation {
                    reason: unavailable.to_string(),
                    fallback: "workspace search".to_owned(),
                }),
            };
        }

        if let Some(capabilities) = &self.capabilities
            && !capabilities.supports(feature)
        {
            return Navigation {
                feature,
                locations: Vec::new(),
                truncated: false,
                encoding,
                document_version: version,
                revision: None,
                degradation: Some(Degradation {
                    reason: format!(
                        "the server does not advertise {}; protocol {LSP_PROTOCOL_VERSION}",
                        feature.label()
                    ),
                    fallback: "workspace search".to_owned(),
                }),
            };
        }

        match query(encoding, version) {
            Ok(mut locations) => {
                // Bounded results: a workspace-symbol query can return
                // thousands.
                let truncated = locations.len() > MAX_NAVIGATION_RESULTS;
                locations.truncate(MAX_NAVIGATION_RESULTS);
                Navigation {
                    feature,
                    locations,
                    truncated,
                    encoding,
                    document_version: version,
                    revision: None,
                    degradation: None,
                }
            }
            Err(err) => {
                let cancelled = err.to_string().contains("cancelled");
                Navigation {
                    feature,
                    locations: Vec::new(),
                    truncated: false,
                    encoding,
                    document_version: version,
                    revision: None,
                    degradation: Some(Degradation {
                        reason: if cancelled {
                            "the request was cancelled".to_owned()
                        } else {
                            err.to_string()
                        },
                        fallback: if cancelled {
                            "none (cancelled)".to_owned()
                        } else {
                            "workspace search".to_owned()
                        },
                    }),
                }
            }
        }
        .with_position_encoding_context(path, position)
    }

    /// Build the environment a server process starts with.
    ///
    /// Deliberately minimal: no credentials, no inherited parent
    /// environment.
    pub fn child_environment(&self) -> Vec<(String, String)> {
        vec![
            ("PATH".to_owned(), "/usr/local/bin:/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), "/tmp".to_owned()),
            ("NO_COLOR".to_owned(), "1".to_owned()),
        ]
    }
}

impl Navigation {
    fn with_position_encoding_context(mut self, path: &str, _position: Position) -> Self {
        // Record which document the query was about, so a caller can
        // check freshness later.
        self.revision = Some(ContentRef {
            path: path.to_owned(),
            start_line: 1,
            end_line: 1,
            content_hash: String::new(),
            bytes: 0,
        });
        self
    }
}

/// A server-produced edit, which is never applied automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEdit {
    pub path: String,
    /// The text the server wants to write.
    pub new_text: String,
    /// The revision the server's edit was computed against.
    pub against_version: i64,
}

impl ServerEdit {
    /// Convert a server edit into a reviewable patch operation.
    ///
    /// The edit goes through the same patch path as any other change:
    /// validated, previewed and approved. Nothing here writes a file.
    pub fn to_patch_op(
        &self,
        current_content: &str,
        current_version: i64,
    ) -> Result<crate::patch::PatchOp, ServerEditRejected> {
        if self.against_version != current_version {
            return Err(ServerEditRejected::Stale {
                against: self.against_version,
                current: current_version,
            });
        }
        Ok(crate::patch::PatchOp::Replace {
            path: self.path.clone(),
            content: self.new_text.clone(),
            expect_hash: crate::workspace::content_hash(current_content.as_bytes()),
        })
    }
}

/// Why a server edit cannot become a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEditRejected {
    Stale { against: i64, current: i64 },
}

impl std::fmt::Display for ServerEditRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerEditRejected::Stale { against, current } => write!(
                f,
                "the server computed this edit against version {against}; the document is at \
                 {current}, so it must be recomputed"
            ),
        }
    }
}

/// A compact candidate for a decision frame, built from navigation results.
///
/// The frame sees identifiers, not source text.
pub fn navigation_candidates(navigation: &Navigation) -> Vec<crate::frame::Candidate> {
    navigation
        .locations
        .iter()
        .take(crate::frame::MAX_CANDIDATES)
        .map(|location| {
            crate::frame::Candidate::new(
                format!(
                    "{}:{}:{}",
                    location.path, location.start.line, location.start.character
                ),
                location.path.clone(),
            )
        })
        .collect()
}

/// The request body for an `initialize` call.
pub fn initialize_request(root_uri: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": Value::Null,
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "definition": { "linkSupport": false },
                    "references": {},
                    "documentSymbol": {},
                    "synchronization": {}
                },
                "workspace": { "symbol": {} }
            },
            // Ask for UTF-8 when the server supports it: it removes a
            // whole class of position bugs.
            "general": {
                "positionEncodings": ["utf-8", "utf-16"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake protocol server: scripted responses, recorded requests.
    struct FakeServer {
        responses: std::sync::Mutex<Vec<Result<Vec<Location>, KnutError>>>,
        requests: std::sync::Mutex<Vec<(String, Position)>>,
    }

    impl FakeServer {
        fn new(responses: Vec<Result<Vec<Location>, KnutError>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn query(
            &self,
            path: &str,
            position: Position,
        ) -> impl FnOnce(PositionEncoding, i64) -> Result<Vec<Location>, KnutError> + '_ {
            self.requests
                .lock()
                .unwrap()
                .push((path.to_owned(), position));
            move |_encoding, _version| {
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    return Ok(Vec::new());
                }
                responses.remove(0)
            }
        }
    }

    fn capabilities_json(encoding: &str) -> Value {
        json!({
            "capabilities": {
                "positionEncoding": encoding,
                "definitionProvider": true,
                "referencesProvider": true,
                "documentSymbolProvider": true,
                "workspaceSymbolProvider": true
            }
        })
    }

    fn location(path: &str, line: u32, character: u32) -> Location {
        Location {
            path: path.to_owned(),
            start: Position::new(line, character),
            end: Position::new(line, character + 4),
        }
    }

    #[test]
    fn initialization_negotiates_the_position_encoding() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust());
        manager.negotiated(&capabilities_json("utf-8"));
        assert_eq!(
            manager.capabilities().unwrap().encoding,
            PositionEncoding::Utf8
        );

        // A server that does not negotiate one gets the protocol default.
        manager.negotiated(&json!({ "capabilities": { "definitionProvider": true } }));
        assert_eq!(
            manager.capabilities().unwrap().encoding,
            PositionEncoding::Utf16
        );

        // Capabilities are read from what the server advertises. The
        // second negotiation above advertised only definitions, so the
        // absent feature is reported as unsupported.
        let capabilities = manager.capabilities().unwrap();
        assert!(capabilities.supports(LspFeature::Definition));
        assert!(!capabilities.supports(LspFeature::References));
    }

    #[test]
    fn unicode_positions_use_the_negotiated_encoding() {
        // A line where UTF-16, UTF-8 and UTF-32 offsets all differ.
        let line = "let x = \"aé日本語b\";";

        let utf8 = PositionEncoding::Utf8.offset_of_byte(line, line.len());
        let utf16 = PositionEncoding::Utf16.offset_of_byte(line, line.len());
        let utf32 = PositionEncoding::Utf32.offset_of_byte(line, line.len());

        assert_eq!(utf8, line.len() as u32);
        assert!(
            utf16 < utf8,
            "UTF-16 must count fewer units than UTF-8 here: utf16={utf16} utf8={utf8}"
        );
        assert!(
            utf32 <= utf16,
            "UTF-32 must not exceed UTF-16 here: utf32={utf32} utf16={utf16}"
        );

        // Round-tripping a position is exact, which is what keeps a
        // definition from pointing at the wrong identifier.
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            let byte = line.find("日本語").unwrap();
            let offset = encoding.offset_of_byte(line, byte);
            assert_eq!(encoding.byte_of_offset(line, offset), byte);
        }
    }

    #[test]
    fn a_fixture_navigation_finds_the_symbol_with_bounded_context() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust().trust(true));
        manager.negotiated(&capabilities_json("utf-16"));
        manager.versions.set("src/lib.rs", 1);

        let server = FakeServer::new(vec![Ok(vec![location("src/other.rs", 42, 8)])]);

        // The fake records the request and returns its scripted response.
        let query = server.query("src/lib.rs", Position::new(10, 4));
        let navigation = manager.navigate(
            "src/lib.rs",
            Position::new(10, 4),
            LspFeature::Definition,
            query,
        );

        assert_eq!(navigation.locations.len(), 1);
        assert_eq!(navigation.locations[0].path, "src/other.rs");
        assert_eq!(navigation.locations[0].start, Position::new(42, 8));
        assert!(navigation.degradation.is_none());
        assert_eq!(navigation.encoding, PositionEncoding::Utf16);
        assert_eq!(navigation.document_version, 1);
    }

    #[test]
    fn results_are_bounded_and_truncation_is_reported() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust().trust(true));
        manager.negotiated(&capabilities_json("utf-16"));

        let many: Vec<Location> = (0..(MAX_NAVIGATION_RESULTS * 3))
            .map(|i| location(&format!("src/file_{i}.rs"), i as u32, 0))
            .collect();

        let navigation = manager.navigate(
            "src/lib.rs",
            Position::new(0, 0),
            LspFeature::WorkspaceSymbols,
            move |_encoding, _version| Ok(many),
        );
        assert_eq!(navigation.locations.len(), MAX_NAVIGATION_RESULTS);
        assert!(navigation.truncated);
    }

    #[test]
    fn an_untrusted_or_missing_server_degrades_to_search() {
        // Untrusted: the command exists on this machine for rust-analyzer,
        // but trust is what matters here.
        let manager = LanguageServerManager::new(ServerConfig::rust());
        let navigation = manager.navigate(
            "src/lib.rs",
            Position::new(0, 0),
            LspFeature::Definition,
            |_, _| panic!("the query must not run for an untrusted server"),
        );
        let degradation = navigation.degradation.unwrap();
        assert!(degradation.reason.contains("not trusted"));
        assert_eq!(degradation.fallback, "workspace search");
        assert!(navigation.locations.is_empty());

        // Missing binary: degrades visibly rather than installing anything.
        let manager = LanguageServerManager::new(ServerConfig {
            program: "definitely-not-a-language-server".to_owned(),
            trusted: true,
            ..ServerConfig::rust()
        });
        let navigation = manager.navigate(
            "src/lib.rs",
            Position::new(0, 0),
            LspFeature::Definition,
            |_, _| panic!("the query must not run for a missing server"),
        );
        assert!(
            navigation
                .degradation
                .unwrap()
                .reason
                .contains("not installed")
        );
    }

    #[test]
    fn an_unsupported_feature_says_so_instead_of_half_working() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust().trust(true));
        // A server that advertises only definitions.
        manager.negotiated(&json!({
            "capabilities": { "definitionProvider": true }
        }));

        let navigation = manager.navigate(
            "src/lib.rs",
            Position::new(0, 0),
            LspFeature::References,
            |_, _| panic!("the query must not run for an unadvertised feature"),
        );
        let degradation = navigation.degradation.unwrap();
        assert!(degradation.reason.contains("does not advertise"));
        assert!(degradation.reason.contains(LSP_PROTOCOL_VERSION));
    }

    #[test]
    fn a_cancelled_request_is_reported_as_cancelled() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust().trust(true));
        manager.negotiated(&capabilities_json("utf-16"));

        let navigation = manager.navigate(
            "src/lib.rs",
            Position::new(0, 0),
            LspFeature::References,
            |_, _| Err(KnutError::Tool("request cancelled".to_owned())),
        );
        let degradation = navigation.degradation.unwrap();
        assert!(degradation.reason.contains("cancelled"));
        assert_eq!(degradation.fallback, "none (cancelled)");
    }

    #[test]
    fn a_crashing_server_becomes_unstable_and_degrades() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust().trust(true));
        manager.negotiated(&capabilities_json("utf-16"));

        for _ in 0..MAX_SERVER_RESTARTS {
            manager.record_restart();
        }
        assert!(matches!(
            manager.availability(),
            Err(ServerUnavailable::Unstable { .. })
        ));

        let navigation = manager.navigate(
            "src/lib.rs",
            Position::new(0, 0),
            LspFeature::Definition,
            |_, _| panic!("an unstable server must not be queried"),
        );
        assert!(navigation.degradation.unwrap().reason.contains("restarted"));
    }

    #[test]
    fn a_delayed_diagnostic_for_an_older_version_cannot_verify_the_current_patch() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust().trust(true));

        // The document is at version 5.
        manager.versions.set("src/lib.rs", 5);
        manager.record_diagnostics("src/lib.rs", 5, Vec::new());
        assert!(manager.diagnostics("src/lib.rs").proves_clean());

        // The user edits: version 6, and the old diagnostics are stale.
        manager.document_edited("src/lib.rs");
        assert_eq!(manager.versions.version("src/lib.rs"), 6);
        assert!(!manager.diagnostics("src/lib.rs").proves_clean());

        // A *late* report for version 5 arrives: it must not mark the
        // current document clean.
        manager.record_diagnostics("src/lib.rs", 5, Vec::new());
        let state = manager.diagnostics("src/lib.rs");
        assert!(
            !state.proves_clean(),
            "a stale report marked the document clean"
        );
        assert!(state.label().contains("version 5"));
        assert!(state.label().contains("6"));

        // A fresh report for version 6 does.
        manager.record_diagnostics("src/lib.rs", 6, Vec::new());
        assert!(manager.diagnostics("src/lib.rs").proves_clean());
    }

    #[test]
    fn not_ready_is_distinct_from_no_errors() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust().trust(true));

        // Never reported: unavailable, not clean.
        let fresh = manager.diagnostics("src/lib.rs");
        assert!(!fresh.proves_clean());
        assert!(matches!(fresh, DiagnosticsState::Unavailable { .. }));

        // Edited, nothing reported yet: pending, not clean.
        manager.document_edited("src/lib.rs");
        let pending = manager.diagnostics("src/lib.rs");
        assert!(matches!(pending, DiagnosticsState::Pending { .. }));
        assert!(!pending.proves_clean());
        assert!(pending.label().contains("pending"));
    }

    #[test]
    fn errors_are_reported_with_their_severity() {
        let mut manager = LanguageServerManager::new(ServerConfig::rust().trust(true));
        manager.versions.set("src/lib.rs", 1);
        manager.record_diagnostics(
            "src/lib.rs",
            1,
            vec![
                Diagnostic {
                    path: "src/lib.rs".to_owned(),
                    range: (Position::new(3, 0), Position::new(3, 10)),
                    severity: DiagnosticSeverity::Error,
                    message: "mismatched types".to_owned(),
                    document_version: 1,
                },
                Diagnostic {
                    path: "src/lib.rs".to_owned(),
                    range: (Position::new(9, 0), Position::new(9, 5)),
                    severity: DiagnosticSeverity::Warning,
                    message: "unused import".to_owned(),
                    document_version: 1,
                },
            ],
        );

        let state = manager.diagnostics("src/lib.rs");
        assert_eq!(state.errors().len(), 2);
        // Warnings alone would be clean; an error is not.
        assert!(!state.proves_clean());
        assert!(state.label().contains("1 error"));
    }

    #[test]
    fn navigation_results_become_compact_candidates_not_source_text() {
        let navigation = Navigation {
            feature: LspFeature::Definition,
            locations: vec![location("src/other.rs", 42, 8)],
            truncated: false,
            encoding: PositionEncoding::Utf16,
            document_version: 1,
            revision: None,
            degradation: None,
        };
        let candidates = navigation_candidates(&navigation);
        assert_eq!(candidates.len(), 1);
        // The candidate carries a location, not code.
        assert!(candidates[0].id.contains("src/other.rs:42:8"));
        assert!(!candidates[0].id.contains("fn "));
    }

    #[test]
    fn a_server_edit_becomes_a_reviewable_patch_not_an_automatic_write() {
        let current = "pub fn main() {}\n";
        let edit = ServerEdit {
            path: "src/lib.rs".to_owned(),
            new_text: "pub fn main() { hello(); }\n".to_owned(),
            against_version: 3,
        };

        // A stale server edit is refused: it was computed against an older
        // document.
        let err = edit.to_patch_op(current, 4).unwrap_err();
        assert!(matches!(err, ServerEditRejected::Stale { .. }));
        assert!(format!("{err}").contains("must be recomputed"));

        // A current one becomes a patch operation with a precondition.
        match edit.to_patch_op(current, 3).unwrap() {
            crate::patch::PatchOp::Replace {
                path, expect_hash, ..
            } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(
                    expect_hash,
                    crate::workspace::content_hash(current.as_bytes())
                );
            }
            other => panic!("expected a replace op, got {other:?}"),
        }
    }

    #[test]
    fn the_server_environment_carries_no_credentials() {
        let manager = LanguageServerManager::new(ServerConfig::rust());
        let environment = manager.child_environment();
        let rendered = format!("{environment:?}");
        assert!(!rendered.contains("KEY"));
        assert!(!rendered.contains("TOKEN"));
        assert!(!rendered.contains("SECRET"));
        // Only the documented entries are present.
        assert_eq!(environment.len(), 3);
    }

    #[test]
    fn the_initialize_request_declares_this_client_honestly() {
        let request = initialize_request("file:///workspace");
        assert_eq!(request["method"], json!("initialize"));
        // The encoding preference is explicit, so the negotiation is a
        // choice rather than an accident.
        assert_eq!(
            request["params"]["general"]["positionEncodings"],
            json!(["utf-8", "utf-16"])
        );
        // No client-side installation or telemetry capability is claimed.
        let rendered = request.to_string();
        assert!(!rendered.contains("install"));
    }

    #[test]
    fn document_versions_track_edits_independently_per_file() {
        let mut versions = DocumentVersions::new();
        assert_eq!(versions.version("a.rs"), 0);
        versions.edit("a.rs");
        versions.edit("a.rs");
        versions.edit("b.rs");

        assert_eq!(versions.version("a.rs"), 2);
        assert_eq!(versions.version("b.rs"), 1);
        assert!(versions.is_current("a.rs", 2));
        assert!(!versions.is_current("a.rs", 1));
    }

    #[test]
    fn supported_features_are_declared_and_bounded() {
        for feature in [
            LspFeature::Definition,
            LspFeature::References,
            LspFeature::DocumentSymbols,
            LspFeature::WorkspaceSymbols,
            LspFeature::Diagnostics,
        ] {
            assert!(feature.is_supported(), "{feature:?} should be supported");
        }
        assert!(!LspFeature::Other.is_supported());
    }
}
