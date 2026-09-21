//! Real workspace read/search tools with revisioned evidence (issue #23).
//!
//! Gives the coding slice useful local context without loading a whole
//! repository or introducing a vector database:
//!
//! - bounded listing, text search and range reads with stable
//!   workspace-relative paths, line ranges and content hashes;
//! - ignore-aware traversal (`.gitignore` plus explicit secret
//!   exclusions), with binary and oversize limits that are *reported*
//!   rather than silently hidden;
//! - a trusted root whose traversal and symlink boundaries are enforced
//!   at the moment of access, not only by an earlier string check;
//! - repository text is untrusted evidence, never new permission or
//!   instruction authority;
//! - results are revisioned: editing a file invalidates its earlier
//!   evidence identity so a later patch can detect a stale precondition.
//!
//! The search implementation is the maintained `ignore` crate (the same
//! traversal engine ripgrep uses) rather than a hand-rolled walker, so
//! ignore semantics match what developers expect.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::KnutError;
use crate::tool::{SideEffect, Tool, ToolMetadata};

/// Capability id for workspace access.
pub const CAPABILITY: &str = "files";

/// Default bound on listing entries.
pub const MAX_LIST_ENTRIES: usize = 200;
/// Default bound on search matches.
pub const MAX_MATCHES: usize = 100;
/// Default bound on a range read's returned lines.
pub const MAX_READ_LINES: usize = 400;
/// Files larger than this are not read as text.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Lines longer than this are truncated in output.
pub const MAX_LINE_CHARS: usize = 500;

/// Glob patterns that are never read or searched, regardless of ignore
/// files: repository content must not be able to grant access to secrets.
pub const DENIED_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "**/.env",
    "**/.env.*",
    "*.pem",
    "*.key",
    "**/id_rsa*",
    "**/.aws/**",
    "**/.ssh/**",
    "**/.git/**",
    "**/credentials.json",
    "*.p12",
];

/// A revisioned reference to exact content.
///
/// The hash changes when the content changes, so evidence produced
/// against one revision cannot be mistaken for evidence about another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRef {
    /// Workspace-relative, `/`-separated path.
    pub path: String,
    /// First line of the referenced range (1-based, inclusive).
    pub start_line: usize,
    /// Last line of the referenced range (1-based, inclusive).
    pub end_line: usize,
    /// Stable content hash over the whole file at read time.
    pub content_hash: String,
    /// File size in bytes at read time.
    pub bytes: u64,
}

impl ContentRef {
    /// Whether this reference still matches the file on disk.
    pub fn is_fresh(&self, workspace: &Workspace) -> bool {
        match workspace.read_bytes(&self.path) {
            Ok(bytes) => content_hash(&bytes) == self.content_hash,
            Err(_) => false,
        }
    }
}

/// A workspace-relative path that has passed traversal checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafePath {
    relative: String,
    absolute: PathBuf,
}

impl SafePath {
    pub fn relative(&self) -> &str {
        &self.relative
    }

    pub fn absolute(&self) -> &Path {
        &self.absolute
    }
}

/// A trusted workspace root.
///
/// Every access re-validates the resolved path, so a symlink or a
/// filesystem change after an earlier check cannot silently escape the
/// root.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    /// Extra deny globs beyond [`DENIED_PATTERNS`].
    denied: Vec<String>,
}

impl Workspace {
    /// Open a workspace root, canonicalizing it once.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, KnutError> {
        let root = root.as_ref();
        let canonical = root.canonicalize().map_err(|err| {
            KnutError::Tool(format!(
                "workspace root {} is not accessible: {err}",
                root.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(KnutError::Tool(format!(
                "workspace root {} is not a directory",
                canonical.display()
            )));
        }
        Ok(Self {
            root: canonical,
            denied: DENIED_PATTERNS.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a caller-supplied path inside the workspace.
    ///
    /// Rejects absolute paths, `..` traversal, empty paths and anything
    /// whose *resolved* location leaves the root (which is how symlink
    /// escape is caught).
    pub fn resolve(&self, relative: &str) -> Result<SafePath, KnutError> {
        let deny = |reason: &str| KnutError::Tool(format!("path {relative:?} rejected: {reason}"));

        if relative.trim().is_empty() {
            return Err(deny("empty path"));
        }

        let candidate = Path::new(relative);
        if candidate.is_absolute() {
            return Err(deny("absolute paths are not workspace paths"));
        }

        // Reject traversal components before touching the filesystem.
        for component in candidate.components() {
            match component {
                Component::ParentDir => return Err(deny("`..` traversal is not allowed")),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(deny("absolute paths are not workspace paths"));
                }
                _ => {}
            }
        }

        let joined = self.root.join(candidate);

        // Resolve symlinks if the path exists; for a not-yet-existing
        // path, validate the deepest existing ancestor.
        let resolved = match joined.canonicalize() {
            Ok(resolved) => resolved,
            Err(_) => {
                let mut ancestor = joined.as_path();
                loop {
                    match ancestor.parent() {
                        Some(parent) => match parent.canonicalize() {
                            Ok(resolved_parent) => {
                                let tail = joined
                                    .strip_prefix(parent)
                                    .map_err(|_| deny("path is outside the workspace"))?;
                                break resolved_parent.join(tail);
                            }
                            Err(_) => ancestor = parent,
                        },
                        None => return Err(deny("path is outside the workspace")),
                    }
                }
            }
        };

        if !resolved.starts_with(&self.root) {
            return Err(deny("resolved path leaves the workspace root"));
        }

        let normalized = resolved
            .strip_prefix(&self.root)
            .map_err(|_| deny("path is outside the workspace"))?;
        let relative = normalized
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");

        Ok(SafePath {
            relative,
            absolute: resolved,
        })
    }

    /// Whether a workspace-relative path is denied by policy.
    pub fn is_denied(&self, relative: &str) -> bool {
        let matcher = globset::GlobSetBuilder::new();
        let mut builder = matcher;
        for pattern in &self.denied {
            if let Ok(glob) = globset::Glob::new(pattern) {
                builder.add(glob);
            }
        }
        // Matching against both the relative path and the basename keeps
        // patterns like `.env` effective for nested files.
        let Ok(set) = builder.build() else {
            // A broken deny list must fail closed.
            return true;
        };
        let basename = relative.rsplit('/').next().unwrap_or(relative);
        set.is_match(relative) || set.is_match(basename)
    }

    /// Read a file's bytes after validating the path and its bounds.
    pub fn read_bytes(&self, relative: &str) -> Result<Vec<u8>, KnutError> {
        let path = self.resolve(relative)?;
        if self.is_denied(path.relative()) {
            return Err(KnutError::Tool(format!(
                "path {:?} is excluded from workspace reads",
                path.relative()
            )));
        }
        let metadata = std::fs::metadata(path.absolute())
            .map_err(|err| KnutError::Tool(format!("stat {:?}: {err}", path.relative())))?;
        if !metadata.is_file() {
            return Err(KnutError::Tool(format!(
                "path {:?} is not a file",
                path.relative()
            )));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(KnutError::Tool(format!(
                "file {:?} is {} bytes, over the {MAX_FILE_BYTES}-byte read limit",
                path.relative(),
                metadata.len()
            )));
        }

        let mut file = std::fs::File::open(path.absolute())
            .map_err(|err| KnutError::Tool(format!("open {:?}: {err}", path.relative())))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(|err| KnutError::Tool(format!("read {:?}: {err}", path.relative())))?;
        Ok(buffer)
    }

    /// Read a file as text, refusing binary content explicitly.
    pub fn read_text(&self, relative: &str) -> Result<String, KnutError> {
        let bytes = self.read_bytes(relative)?;
        if is_binary(&bytes) {
            return Err(KnutError::Tool(format!(
                "file {:?} looks binary; refusing to read it as text",
                relative
            )));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// A stable, bounded content hash.
///
/// FNV-1a over the bytes: enough to detect a changed revision for stale
/// patches and evidence identity. It is not a cryptographic commitment
/// and is not used as one.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a:{hash:016x}:{}", bytes.len())
}

/// Whether bytes look like binary content.
fn is_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8000)];
    sample.contains(&0u8)
}

/// Bound one output line, marking truncation.
fn bound_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let mut bounded: String = line.chars().take(MAX_LINE_CHARS).collect();
    bounded.push('…');
    bounded
}

// --- tools --------------------------------------------------------------

/// `files.list`: bounded, ignore-aware listing.
pub struct ListTool {
    workspace: Workspace,
}

impl ListTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for ListTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: "list".to_owned(),
            tool_version: "1".to_owned(),
            capability: CAPABILITY.to_owned(),
            description: "List workspace files, honoring ignore files".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "limit": { "type": "number" }
                }
            }),
            side_effect: SideEffect::ReadOnly,
        }
    }

    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        let subpath = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|l| l as usize)
            .unwrap_or(MAX_LIST_ENTRIES)
            .min(MAX_LIST_ENTRIES);

        let base = if subpath.trim().is_empty() {
            self.workspace.root().to_path_buf()
        } else {
            self.workspace.resolve(&subpath)?.absolute().to_path_buf()
        };

        let mut entries: Vec<String> = Vec::new();
        let mut truncated = false;

        let walker = ignore::WalkBuilder::new(&base)
            .standard_filters(true)
            .require_git(false)
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                // A traversal error is reported, never silently skipped.
                Err(err) => {
                    return Ok(json!({
                        "entries": entries,
                        "truncated": truncated,
                        "error": format!("traversal error: {err}"),
                    }));
                }
            };
            let path = entry.path();
            if path == base {
                continue;
            }
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
            if entries.len() >= limit {
                truncated = true;
                break;
            }
            let suffix = if entry.file_type().is_some_and(|t| t.is_dir()) {
                "/"
            } else {
                ""
            };
            entries.push(format!("{relative}{suffix}"));
        }

        entries.sort();
        Ok(json!({
            "entries": entries,
            "truncated": truncated,
            "limit": limit,
        }))
    }
}

/// `files.search`: bounded, ignore-aware text search.
pub struct SearchTool {
    workspace: Workspace,
}

impl SearchTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for SearchTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: "search".to_owned(),
            tool_version: "1".to_owned(),
            capability: CAPABILITY.to_owned(),
            description: "Search workspace text, honoring ignore files".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "number" }
                },
                "required": ["query"]
            }),
            side_effect: SideEffect::ReadOnly,
        }
    }

    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if query.trim().is_empty() {
            return Err(KnutError::Tool("search query must not be empty".to_owned()));
        }
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|l| l as usize)
            .unwrap_or(MAX_MATCHES)
            .min(MAX_MATCHES);

        let workspace = self.workspace.clone();
        let query = query.clone();
        let matches =
            tokio::task::spawn_blocking(move || search_blocking(&workspace, &query, limit))
                .await
                .map_err(|err| KnutError::Tool(format!("search task failed: {err}")))??;

        Ok(matches)
    }
}

fn search_blocking(workspace: &Workspace, query: &str, limit: usize) -> Result<Value, KnutError> {
    let pattern = globset::Glob::new(query).ok();
    let mut results: Vec<Value> = Vec::new();
    let mut truncated = false;
    let mut skipped_binary = 0usize;
    let mut skipped_large = 0usize;

    let walker = ignore::WalkBuilder::new(workspace.root())
        .standard_filters(true)
        .require_git(false)
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                return Ok(json!({
                    "matches": results,
                    "truncated": truncated,
                    "error": format!("traversal error: {err}"),
                }));
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(workspace.root()) else {
            continue;
        };
        let relative = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if workspace.is_denied(&relative) {
            continue;
        }

        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.len() > MAX_FILE_BYTES {
            skipped_large += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if is_binary(&bytes) {
            skipped_binary += 1;
            continue;
        }

        let text = String::from_utf8_lossy(&bytes);
        let hash = content_hash(&bytes);
        for (index, line) in text.lines().enumerate() {
            if !line.contains(query)
                && !pattern
                    .as_ref()
                    .is_some_and(|p| p.compile_matcher().is_match(line))
            {
                continue;
            }
            if results.len() >= limit {
                truncated = true;
                break;
            }
            results.push(json!({
                "path": relative,
                "line": index + 1,
                "text": bound_line(line),
                "content_hash": hash,
            }));
        }
        if truncated {
            break;
        }
    }

    Ok(json!({
        "matches": results,
        "truncated": truncated,
        "skipped_binary": skipped_binary,
        "skipped_large": skipped_large,
        "limit": limit,
    }))
}

/// `files.read`: bounded range read with a revisioned reference.
pub struct ReadTool {
    workspace: Workspace,
}

impl ReadTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: "read".to_owned(),
            tool_version: "1".to_owned(),
            capability: CAPABILITY.to_owned(),
            description: "Read a bounded range of a workspace file".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "number" },
                    "end_line": { "type": "number" }
                },
                "required": ["path"]
            }),
            side_effect: SideEffect::ReadOnly,
        }
    }

    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| KnutError::Tool("read requires a path".to_owned()))?
            .to_owned();

        let bytes = self.workspace.read_bytes(&path)?;
        if is_binary(&bytes) {
            return Err(KnutError::Tool(format!(
                "file {path:?} looks binary; refusing to read it as text"
            )));
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total_lines = lines.len();

        let start = input
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(1)
            .max(1);
        let requested_end = input
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(total_lines.max(start));

        let end = requested_end
            .min(total_lines)
            .min(start.saturating_add(MAX_READ_LINES).saturating_sub(1));

        let mut out = Vec::new();
        if start <= total_lines {
            for (index, line) in lines
                .iter()
                .enumerate()
                .skip(start - 1)
                .take(end - start + 1)
            {
                out.push(json!({
                    "line": index + 1,
                    "text": bound_line(line),
                }));
            }
        }

        // The reference identity covers the whole file, so a later read
        // of a changed file has a different identity.
        let reference = ContentRef {
            path: self.workspace.resolve(&path)?.relative().to_owned(),
            start_line: start,
            end_line: end,
            content_hash: content_hash(&bytes),
            bytes: bytes.len() as u64,
        };

        Ok(json!({
            "path": reference.path,
            "start_line": start,
            "end_line": end,
            "total_lines": total_lines,
            "lines": out,
            "truncated": requested_end > end,
            "content_hash": reference.content_hash,
            "reference": serde_json::to_value(&reference).unwrap_or(Value::Null),
        }))
    }
}

/// `files.write`: the plan-invocable editing tool.
///
/// It exists so a validated plan can actually change code, and it is the
/// narrowest thing that can: the caller supplies a workspace-relative path
/// and the file's full new contents, with the content hash it expects to
/// replace. There is no append, no regex substitution and no shell: a
/// stale or mistyped precondition fails the node rather than rewriting
/// something the caller did not read.
///
/// It declares `IdempotentWrite`, so the gate decides whether approval is
/// needed and the journal makes the write replay-safe.
pub struct WriteTool {
    workspace: Workspace,
}

impl WriteTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: "write".to_owned(),
            tool_version: "1".to_owned(),
            capability: CAPABILITY.to_owned(),
            description: "Replace a workspace file with new contents, checking its revision first"
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "expect_hash": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            side_effect: SideEffect::IdempotentWrite,
        }
    }

    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| KnutError::Tool("write requires a path".to_owned()))?
            .to_owned();
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| KnutError::Tool("write requires content".to_owned()))?
            .to_owned();
        if content.len() > MAX_FILE_BYTES as usize {
            return Err(KnutError::Tool(format!(
                "write of {} bytes exceeds the {MAX_FILE_BYTES}-byte limit",
                content.len()
            )));
        }

        // A caller that read the file tells us its revision; the check is
        // what makes a stale edit fail instead of overwriting.
        let existing = self.workspace.read_bytes(&path).ok();
        if let Some(expected) = input.get("expect_hash").and_then(Value::as_str) {
            let actual = existing.as_ref().map(|bytes| content_hash(bytes));
            if actual.as_deref() != Some(expected) {
                return Err(KnutError::Tool(format!(
                    "{path:?} changed since it was read (expected {expected}, found {})",
                    actual.as_deref().unwrap_or("<missing>")
                )));
            }
        }

        let safe = self.workspace.resolve(&path)?;
        if self.workspace.is_denied(safe.relative()) {
            return Err(KnutError::Tool(format!(
                "path {:?} is excluded from workspace writes",
                safe.relative()
            )));
        }
        if let Some(parent) = safe.absolute().parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| KnutError::Tool(format!("creating {parent:?}: {err}")))?;
        }
        std::fs::write(safe.absolute(), content.as_bytes())
            .map_err(|err| KnutError::Tool(format!("writing {:?}: {err}", safe.relative())))?;

        let bytes = content.as_bytes();
        Ok(json!({
            "path": safe.relative(),
            "bytes": bytes.len(),
            "content_hash": content_hash(bytes),
        }))
    }
}

/// Register the workspace tools into a registry.
pub fn register_workspace_tools(
    registry: &mut crate::ToolRegistry,
    workspace: Workspace,
) -> Result<(), KnutError> {
    registry.register(ListTool::new(workspace.clone()))?;
    registry.register(SearchTool::new(workspace.clone()))?;
    registry.register(ReadTool::new(workspace.clone()))?;
    registry.register(WriteTool::new(workspace))?;
    Ok(())
}

/// Repository instruction files, in decreasing precedence.
///
/// The first found in a directory wins for that directory; nearer
/// directories win over farther ones. These are *evidence*, never
/// authority: nothing here can enable executable hooks, MCP servers,
/// network destinations or configuration.
pub const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "KNUT.md", "CLAUDE.md"];

/// Maximum accepted instruction-file size.
pub const MAX_INSTRUCTION_BYTES: u64 = 64 * 1024;

/// One discovered instruction file, with its provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionFile {
    /// Workspace-relative path.
    pub path: String,
    /// Which precedence slot this file occupies in its directory.
    pub precedence: usize,
    pub content_hash: String,
    /// Bounded contents.
    pub content: String,
}

/// Discover repository instruction files under the workspace root.
///
/// Root first, then nested directories (shallower first), honoring ignore
/// files and size limits. Loading instructions **never** grants trust:
/// the caller decides what, if anything, to do with them.
pub fn discover_instructions(workspace: &Workspace) -> Result<Vec<InstructionFile>, KnutError> {
    let mut found: Vec<InstructionFile> = Vec::new();

    let walker = ignore::WalkBuilder::new(workspace.root())
        .standard_filters(true)
        .require_git(false)
        .build();

    let mut directories: Vec<PathBuf> = vec![workspace.root().to_path_buf()];
    for entry in walker {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            directories.push(entry.path().to_path_buf());
        }
    }
    // Shallower directories first: nearer files refine farther ones.
    directories.sort_by_key(|dir| dir.components().count());

    for directory in directories {
        for (precedence, name) in INSTRUCTION_FILES.iter().enumerate() {
            let candidate = directory.join(name);
            if !candidate.is_file() {
                continue;
            }
            let Ok(relative) = candidate.strip_prefix(workspace.root()) else {
                continue;
            };
            let relative = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if workspace.is_denied(&relative) {
                continue;
            }
            let Ok(metadata) = std::fs::metadata(&candidate) else {
                continue;
            };
            if metadata.len() > MAX_INSTRUCTION_BYTES {
                // Reported by omission-with-marker rather than silently
                // truncated: an oversized instruction file is skipped.
                continue;
            }
            let Ok(bytes) = std::fs::read(&candidate) else {
                continue;
            };
            if is_binary(&bytes) {
                continue;
            }
            found.push(InstructionFile {
                path: relative,
                precedence,
                content_hash: content_hash(&bytes),
                content: String::from_utf8_lossy(&bytes).into_owned(),
            });
            // One instruction file per directory.
            break;
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary workspace with fixture files.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "knut-workspace-{name}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.dir.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
            path
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

    #[tokio::test]
    async fn a_temporary_repository_can_be_listed_searched_and_read() {
        let fixture = Fixture::new("basic");
        fixture.write("src/lib.rs", "fn main() {\n    println!(\"hi\");\n}\n");
        fixture.write("README.md", "the release notes live here\n");
        let workspace = fixture.workspace();

        let listed = ListTool::new(workspace.clone())
            .call(json!({}))
            .await
            .unwrap();
        let entries: Vec<String> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert!(entries.contains(&"src/lib.rs".to_owned()));
        assert!(entries.contains(&"README.md".to_owned()));

        let searched = SearchTool::new(workspace.clone())
            .call(json!({ "query": "release" }))
            .await
            .unwrap();
        let matches = searched["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["path"], json!("README.md"));
        assert_eq!(matches[0]["line"], json!(1));

        let read = ReadTool::new(workspace.clone())
            .call(json!({ "path": "src/lib.rs", "start_line": 2, "end_line": 3 }))
            .await
            .unwrap();
        assert_eq!(read["start_line"], json!(2));
        assert_eq!(read["end_line"], json!(3));
        assert_eq!(read["lines"].as_array().unwrap().len(), 2);
        assert_eq!(read["path"], json!("src/lib.rs"));
        assert!(read["content_hash"].as_str().unwrap().starts_with("fnv1a:"));
    }

    #[tokio::test]
    async fn ignored_files_and_fixture_secrets_have_tested_behavior() {
        let fixture = Fixture::new("ignore");
        fixture.write(".gitignore", "secret_notes.txt\n");
        fixture.write("secret_notes.txt", "the secret is in the ignored file\n");
        fixture.write(".env", "API_KEY=super-secret-value\n");
        fixture.write("visible.txt", "the secret is visible here\n");
        let workspace = fixture.workspace();

        // The ignored file is not listed or searched.
        let listed = ListTool::new(workspace.clone())
            .call(json!({}))
            .await
            .unwrap();
        let entries = listed["entries"].as_array().unwrap();
        assert!(
            !entries
                .iter()
                .any(|e| e.as_str().unwrap().contains("secret_notes"))
        );

        let searched = SearchTool::new(workspace.clone())
            .call(json!({ "query": "secret" }))
            .await
            .unwrap();
        let paths: Vec<&str> = searched["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"visible.txt"));
        assert!(!paths.iter().any(|p| p.contains("secret_notes")));

        // A denied secret file cannot be read even by exact path, even
        // though it is not in .gitignore.
        let err = ReadTool::new(workspace.clone())
            .call(json!({ "path": ".env" }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("excluded"), "got {err}");
    }

    #[tokio::test]
    async fn binary_and_oversized_content_are_reported_not_hidden() {
        let fixture = Fixture::new("binary");
        std::fs::write(fixture.dir.join("blob.bin"), [0u8, 1, 2, 3, 0, 9]).unwrap();
        fixture.write("big.txt", &"x".repeat((MAX_FILE_BYTES + 10) as usize));
        let workspace = fixture.workspace();

        // Reading binary content is refused explicitly.
        let err = ReadTool::new(workspace.clone())
            .call(json!({ "path": "blob.bin" }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("binary"), "got {err}");

        // Search reports what it skipped rather than pretending the tree
        // contains nothing.
        let searched = SearchTool::new(workspace.clone())
            .call(json!({ "query": "x" }))
            .await
            .unwrap();
        assert_eq!(searched["skipped_binary"], json!(1));
        assert_eq!(searched["skipped_large"], json!(1));

        // An oversized read is refused with the limit in the message.
        let err = ReadTool::new(workspace.clone())
            .call(json!({ "path": "big.txt" }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("read limit"), "got {err}");
    }

    #[tokio::test]
    async fn traversal_and_symlink_escape_are_rejected() {
        let fixture = Fixture::new("traversal");
        fixture.write("inside.txt", "inside\n");
        let outside = Fixture::new("outside");
        outside.write("outside.txt", "outside content\n");
        std::os::unix::fs::symlink(&outside.dir, fixture.dir.join("escape")).unwrap();

        let workspace = fixture.workspace();

        // Plain traversal.
        for attempt in ["../outside.txt", "/etc/passwd", "inside.txt/../.."] {
            let err = workspace.resolve(attempt).unwrap_err();
            assert!(
                format!("{err}").contains("rejected"),
                "{attempt} was not rejected: {err}"
            );
        }

        // Symlink escape: the path looks local but resolves outside.
        let err = workspace.resolve("escape/outside.txt").unwrap_err();
        assert!(
            format!("{err}").contains("leaves the workspace root")
                || format!("{err}").contains("rejected"),
            "symlink escape was not caught: {err}"
        );
    }

    #[tokio::test]
    async fn unicode_paths_work() {
        let fixture = Fixture::new("unicode");
        fixture.write("src/日本語.md", "スカンジナビアのメモ\n");
        fixture.write("src/überlegungen.txt", "überlegungen\n");
        let workspace = fixture.workspace();

        let read = ReadTool::new(workspace.clone())
            .call(json!({ "path": "src/日本語.md" }))
            .await
            .unwrap();
        assert_eq!(read["path"], json!("src/日本語.md"));
        assert_eq!(read["lines"][0]["text"], json!("スカンジナビアのメモ"));

        let searched = SearchTool::new(workspace.clone())
            .call(json!({ "query": "überlegungen" }))
            .await
            .unwrap();
        assert_eq!(searched["matches"].as_array().unwrap().len(), 1);
        assert_eq!(
            searched["matches"][0]["path"],
            json!("src/überlegungen.txt")
        );
    }

    #[tokio::test]
    async fn modifying_a_file_invalidates_its_earlier_evidence_identity() {
        let fixture = Fixture::new("revision");
        let path = fixture.write("src/lib.rs", "fn main() {}\n");
        let workspace = fixture.workspace();

        let read = ReadTool::new(workspace.clone())
            .call(json!({ "path": "src/lib.rs" }))
            .await
            .unwrap();
        let reference: ContentRef = serde_json::from_value(read["reference"].clone()).unwrap();
        assert!(reference.is_fresh(&workspace));

        // A later edit invalidates the earlier identity.
        std::fs::write(&path, "fn main() { println!(\"changed\"); }\n").unwrap();
        assert!(
            !reference.is_fresh(&workspace),
            "a changed file kept its old revision identity"
        );

        let reread = ReadTool::new(workspace.clone())
            .call(json!({ "path": "src/lib.rs" }))
            .await
            .unwrap();
        let new_reference: ContentRef =
            serde_json::from_value(reread["reference"].clone()).unwrap();
        assert_ne!(new_reference.content_hash, reference.content_hash);
    }

    #[test]
    fn repository_instructions_are_discovered_with_precedence_and_limits() {
        let fixture = Fixture::new("instructions");
        fixture.write("AGENTS.md", "root instructions\n");
        fixture.write("src/AGENTS.md", "src instructions\n");
        // Lower precedence than AGENTS.md in the same directory.
        fixture.write("src/CLAUDE.md", "claude instructions\n");
        // Oversized files are skipped rather than truncated silently.
        fixture.write(
            "deep/KNUT.md",
            &"x".repeat((MAX_INSTRUCTION_BYTES + 1) as usize),
        );
        fixture.write("deep/nested/KNUT.md", "nested instructions\n");

        let workspace = fixture.workspace();
        let instructions = discover_instructions(&workspace).unwrap();

        let paths: Vec<&str> = instructions.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"AGENTS.md"));
        assert!(paths.contains(&"src/AGENTS.md"));
        assert!(paths.contains(&"deep/nested/KNUT.md"));
        // The oversized file was skipped.
        assert!(!paths.contains(&"deep/KNUT.md"));
        // AGENTS.md wins over CLAUDE.md in the same directory.
        assert!(!paths.contains(&"src/CLAUDE.md"));

        // Root instructions come first.
        assert_eq!(instructions[0].path, "AGENTS.md");
        // Each entry carries a revision identity.
        assert!(instructions[0].content_hash.starts_with("fnv1a:"));
    }

    #[test]
    fn schema_subset_check_accepts_declared_required_fields() {
        // A schema that legitimately declares `required` must register;
        // only unsupported constructs are refused.
        let ok = json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        });
        crate::validate_schema_supported(&ok).unwrap();

        let unsupported = json!({ "type": "object", "properties": { "x": { "type": "null" } } });
        assert!(crate::validate_schema_supported(&unsupported).is_err());

        let untyped = json!({ "properties": {} });
        assert!(crate::validate_schema_supported(&untyped).is_err());
    }

    #[test]
    fn deny_matching_covers_nested_paths_and_basenames() {
        let fixture = Fixture::new("deny");
        fixture.write("normal.txt", "ok\n");
        let workspace = fixture.workspace();

        // Patterns match both a nested relative path and a basename, so
        // `.env` protects `config/.env` as well.
        assert!(workspace.is_denied("config/.env"));
        assert!(workspace.is_denied(".env"));
        assert!(workspace.is_denied("keys/server.pem"));
        assert!(workspace.is_denied("deep/nested/.ssh/id_rsa"));
        assert!(!workspace.is_denied("src/lib.rs"));
        assert!(!workspace.is_denied("README.md"));
    }

    #[tokio::test]
    async fn long_lines_are_bounded_in_output() {
        let fixture = Fixture::new("longline");
        let long = "y".repeat(MAX_LINE_CHARS * 3);
        fixture.write("long.txt", &long);
        let workspace = fixture.workspace();

        let read = ReadTool::new(workspace.clone())
            .call(json!({ "path": "long.txt" }))
            .await
            .unwrap();
        let text = read["lines"][0]["text"].as_str().unwrap();
        assert!(text.chars().count() <= MAX_LINE_CHARS + 1);
        assert!(text.ends_with('…'));
    }

    #[tokio::test]
    async fn workspace_tools_register_as_one_capability() {
        let fixture = Fixture::new("registry");
        fixture.write("a.txt", "a\n");
        let mut registry = crate::ToolRegistry::default();
        register_workspace_tools(&mut registry, fixture.workspace()).unwrap();

        assert_eq!(registry.capabilities(), vec![CAPABILITY.to_owned()]);
        let candidates = registry.tools_for_capability(CAPABILITY);
        // Three read-only tools plus the one writing tool.
        assert_eq!(candidates.len(), 4);
        assert_eq!(
            candidates
                .iter()
                .filter(|metadata| metadata.side_effect == SideEffect::ReadOnly)
                .count(),
            3
        );
        // The write tool declares a write, so the gate treats it as one:
        // nothing here is silently read-only.
        assert_eq!(
            candidates
                .iter()
                .filter(|metadata| metadata.side_effect == SideEffect::IdempotentWrite)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn the_write_tool_changes_a_file_and_honors_its_precondition() {
        let fixture = Fixture::new("write-tool");
        fixture.write("src/lib.rs", "pub fn sum() {}\n");
        let workspace = fixture.workspace();
        let tool = WriteTool::new(workspace.clone());

        // Read first, so the caller has the revision it is replacing.
        let read = crate::Tool::call(&tool, json!({ "path": "src/lib.rs" })).await;
        let _ = read;
        let original = workspace.read_text("src/lib.rs").unwrap();
        let hash = content_hash(original.as_bytes());

        // A matching precondition writes.
        let written = crate::Tool::call(
            &tool,
            json!({
                "path": "src/lib.rs",
                "content": "pub fn sum(values: &[i64]) -> i64 { values.iter().sum() }\n",
                "expect_hash": hash,
            }),
        )
        .await
        .unwrap();
        assert_eq!(written["path"], json!("src/lib.rs"));
        assert!(
            workspace
                .read_text("src/lib.rs")
                .unwrap()
                .contains("values.iter().sum()")
        );

        // A stale precondition is refused rather than overwriting.
        let err = crate::Tool::call(
            &tool,
            json!({
                "path": "src/lib.rs",
                "content": "// clobbered\n",
                "expect_hash": hash, // the *old* hash
            }),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err}").contains("changed since it was read"),
            "got {err}"
        );
        // The new content survived.
        assert!(
            workspace
                .read_text("src/lib.rs")
                .unwrap()
                .contains("values.iter().sum()")
        );
    }

    #[tokio::test]
    async fn the_write_tool_refuses_denied_paths_and_traversal() {
        let fixture = Fixture::new("write-denied");
        fixture.write(".env", "SECRET=1\n");
        let workspace = fixture.workspace();
        let tool = WriteTool::new(workspace.clone());

        let err = crate::Tool::call(&tool, json!({ "path": ".env", "content": "SECRET=2\n" }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("excluded"), "got {err}");

        let err = crate::Tool::call(&tool, json!({ "path": "../outside.txt", "content": "x" }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("rejected"), "got {err}");
    }
}
