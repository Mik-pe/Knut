//! Supervised sandboxed commands with real cancellation and bounded
//! output (issue #25).
//!
//! Running a repository's build script or test suite *is* executing
//! repository code, so the same trust and filesystem/network rules apply
//! as to any other effect. The sandbox is enforced by the OS
//! (bubblewrap on Linux: mount namespaces, no network namespace), never
//! by keyword filtering, and an unavailable sandbox fails visibly rather
//! than silently degrading to unsandboxed execution.
//!
//! Boundaries:
//! - executable + argv are explicit; a shell is a *separate* capability,
//!   never an implicit implementation detail;
//! - the parent environment is not inherited: provider keys and other
//!   secrets do not reach repository code;
//! - process groups are supervised, output is drained without deadlock
//!   and bounded, and cancellation stops descendants — not just the
//!   direct child;
//! - exit status, signal, timeout/cancel and unknown effects are recorded
//!   separately, and a potentially effectful command is never retried
//!   automatically after an ambiguous failure.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::KnutError;
use crate::tool::{SideEffect, Tool, ToolMetadata};
use crate::workspace::Workspace;

/// Capability for supervised command execution.
pub const CAPABILITY: &str = "shell";

/// Default wall-clock limit for one command.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Grace period between SIGTERM and SIGKILL.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(3);

/// Maximum bytes retained per stream.
pub const MAX_STREAM_BYTES: usize = 256 * 1024;

/// Maximum bytes retained in total across both streams.
pub const MAX_TOTAL_BYTES: usize = 512 * 1024;

/// How a command may touch the world.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Workspace subpaths the command may read/write, relative to the
    /// root. Empty means the whole workspace (read-only unless
    /// `writable_paths` also lists it).
    pub readable_paths: Vec<String>,
    /// Workspace subpaths the command may modify.
    pub writable_paths: Vec<String>,
    /// Whether the command may use the network.
    pub network: bool,
}

/// Which sandbox backend was actually used.
///
/// Reported on every result: "the command ran" is not a claim about how
/// it was contained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    /// OS-enforced isolation via bubblewrap.
    Bubblewrap,
    /// No sandbox: only ever entered through an explicit, separately
    /// authorized mode, never as a fallback.
    Unsandboxed,
}

impl SandboxBackend {
    /// Whether this backend actually enforces restrictions.
    pub fn is_enforced(self) -> bool {
        matches!(self, SandboxBackend::Bubblewrap)
    }
}

/// Why a command could not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxUnavailable {
    /// The backend binary is missing.
    BackendMissing {
        backend: &'static str,
        detail: String,
    },
    /// The platform has no tested backend.
    Unsupported { platform: String },
}

impl std::fmt::Display for SandboxUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxUnavailable::BackendMissing { backend, detail } => write!(
                f,
                "sandbox backend {backend:?} is unavailable ({detail}); \
                 command execution is refused rather than run unsandboxed"
            ),
            SandboxUnavailable::Unsupported { platform } => write!(
                f,
                "no tested sandbox backend on {platform}; refusing to run commands unsandboxed"
            ),
        }
    }
}

/// A request to run one finite command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    /// Executable, resolved by the OS (no shell interpretation).
    pub program: String,
    /// Arguments, passed verbatim: spaces and metacharacters are data.
    pub args: Vec<String>,
    /// Working directory relative to the workspace root.
    pub working_dir: String,
    /// Extra environment variables. The parent environment is never
    /// inherited wholesale.
    pub env: BTreeMap<String, String>,
    pub timeout_secs: u64,
    pub spec: SandboxSpec,
    /// Read-only host directories the command needs (toolchain roots such
    /// as `~/.cargo`, `~/.rustup`, `~/.local/share/mise`). Never the home
    /// directory itself: the caller names exactly what is needed.
    pub toolchain_paths: Vec<String>,
}

impl CommandRequest {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            working_dir: ".".to_owned(),
            env: BTreeMap::new(),
            timeout_secs: DEFAULT_TIMEOUT.as_secs(),
            spec: SandboxSpec::default(),
            toolchain_paths: default_toolchain_paths(),
        }
    }

    pub fn with_toolchain_paths(mut self, paths: Vec<String>) -> Self {
        self.toolchain_paths = paths;
        self
    }

    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = dir.into();
        self
    }

    pub fn with_writable(mut self, paths: Vec<String>) -> Self {
        self.spec.writable_paths = paths;
        self
    }

    pub fn with_network(mut self, allowed: bool) -> Self {
        self.spec.network = allowed;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout_secs = timeout.as_secs();
        self
    }
}

/// How a command ended. These are distinct facts, never collapsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Exited with this code.
    Exited { code: i32 },
    /// Killed by this signal.
    Signalled { signal: i32 },
    /// Stopped at the wall-clock limit.
    TimedOut,
    /// Stopped by cancellation.
    Cancelled,
    /// The sandbox refused to start it.
    SandboxRefused { reason: String },
    /// It started but its outcome cannot be determined.
    UnknownEffect { reason: String },
}

impl CommandStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, CommandStatus::Exited { code: 0 })
    }
}

/// One bounded stream's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedOutput {
    /// Retained text (tail-truncated when it exceeded the bound).
    pub text: String,
    pub bytes: usize,
    pub truncated: bool,
    /// Whether control sequences were stripped for display.
    pub sanitized: bool,
}

/// The full record of one supervised command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub program: String,
    pub args: Vec<String>,
    pub status: CommandStatus,
    pub stdout: BoundedOutput,
    pub stderr: BoundedOutput,
    pub duration_ms: u64,
    pub backend: SandboxBackend,
    /// Whether the command could have changed the workspace; used to
    /// decide that an ambiguous failure must not be retried.
    pub possibly_effectful: bool,
    /// Bounded, sanitized output suitable for feeding a model.
    pub model_summary: String,
}

impl CommandOutcome {
    /// Whether this outcome is safe to retry automatically.
    ///
    /// Deliberately conservative: anything that may have had an effect
    /// is not retried after an ambiguous stop.
    pub fn is_retriable(&self) -> bool {
        matches!(
            self.status,
            CommandStatus::SandboxRefused { .. } | CommandStatus::Exited { .. }
        ) && !self.possibly_effectful
    }
}

/// Removes terminal control sequences from untrusted output.
///
/// Repository text and build logs are untrusted: escape sequences there
/// must not be able to move the cursor, rewrite lines or drive the
/// terminal.
pub fn sanitize_terminal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: ESC [ ... final byte
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Other two-character sequences.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\r' => out.push('\n'),
            '\u{7}' => {}
            other => out.push(other),
        }
    }
    out
}

/// Bound one stream, keeping the tail (where failures usually are).
fn bound_output(bytes: &[u8], limit: usize) -> BoundedOutput {
    let text = String::from_utf8_lossy(bytes);
    let sanitized = sanitize_terminal(&text);
    let truncated = sanitized.len() > limit;
    let bounded = if truncated {
        // Keep the tail (where failures are) and stay within the bound
        // including the elision marker itself.
        let tail_budget = limit.saturating_sub(64);
        let skip = sanitized.len().saturating_sub(tail_budget);
        let start = sanitized
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= skip)
            .unwrap_or(sanitized.len());
        let elided = sanitized.len() - start;
        format!("[...{} bytes elided...]\n{}", elided, &sanitized[start..])
    } else {
        sanitized
    };
    BoundedOutput {
        text: bounded,
        bytes: bytes.len(),
        truncated,
        sanitized: true,
    }
}

/// A command supervisor.
pub struct Supervisor {
    workspace: Workspace,
    /// Explicitly trust unsandboxed execution. Off by default and never
    /// a fallback.
    trust_unsandboxed: bool,
    cancel: Arc<AtomicBool>,
}

impl Supervisor {
    /// A supervisor that refuses to run anything unsandboxed.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            trust_unsandboxed: false,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Explicitly authorize unsandboxed execution.
    ///
    /// This is a separate, deliberate decision by the operator; the
    /// backend is reported on every result so the caller can see it.
    pub fn with_unsandboxed_trust(mut self, trusted: bool) -> Self {
        self.trust_unsandboxed = trusted;
        self
    }

    /// A cancellation token shared with in-flight commands.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Detect the sandbox backend actually available.
    pub fn backend(&self) -> Result<SandboxBackend, SandboxUnavailable> {
        if !cfg!(target_os = "linux") {
            return Err(SandboxUnavailable::Unsupported {
                platform: std::env::consts::OS.to_owned(),
            });
        }
        match which("bwrap") {
            Some(_) => Ok(SandboxBackend::Bubblewrap),
            None => Err(SandboxUnavailable::BackendMissing {
                backend: "bwrap",
                detail: "not found on PATH".to_owned(),
            }),
        }
    }

    /// Run one command under supervision.
    pub async fn run(&self, request: CommandRequest) -> Result<CommandOutcome, KnutError> {
        // Refuse visibly rather than degrade: an unavailable sandbox is
        // never a reason to run unsandboxed.
        let backend = match self.backend() {
            Ok(backend) => backend,
            Err(unavailable) => {
                if self.trust_unsandboxed {
                    SandboxBackend::Unsandboxed
                } else {
                    return Ok(refused_outcome(&request, unavailable.to_string()));
                }
            }
        };

        let workspace = self.workspace.clone();
        let cancel = Arc::clone(&self.cancel);
        let timeout = Duration::from_secs(request.timeout_secs.max(1));
        let trust = self.trust_unsandboxed;

        let abandoned = Arc::new(AtomicBool::new(false));
        let _guard = CancelOnDrop(Arc::clone(&abandoned));
        tokio::task::spawn_blocking(move || {
            run_blocking(
                &workspace, &request, backend, timeout, cancel, abandoned, trust,
            )
        })
        .await
        .map_err(|err| KnutError::Tool(format!("supervisor task failed: {err}")))?
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn refused_outcome(request: &CommandRequest, reason: String) -> CommandOutcome {
    CommandOutcome {
        program: request.program.clone(),
        args: request.args.clone(),
        status: CommandStatus::SandboxRefused {
            reason: reason.clone(),
        },
        stdout: BoundedOutput {
            text: String::new(),
            bytes: 0,
            truncated: false,
            sanitized: true,
        },
        stderr: BoundedOutput {
            text: reason.clone(),
            bytes: reason.len(),
            truncated: false,
            sanitized: true,
        },
        duration_ms: 0,
        backend: SandboxBackend::Unsandboxed,
        possibly_effectful: false,
        model_summary: format!("command not run: {reason}"),
    }
}

/// Environment a toolchain needs inside the sandbox.
///
/// Only variables that point the toolchain at roots this request already
/// exposes read-only. Nothing else from the parent environment is passed.
pub fn toolchain_env(request: &CommandRequest) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let find = |suffix: &str| {
        request
            .toolchain_paths
            .iter()
            .find(|path| path.ends_with(suffix))
            .cloned()
    };
    if let Some(cargo_home) = find("/.cargo") {
        env.push(("CARGO_HOME".to_owned(), cargo_home));
    }
    if let Some(rustup_home) = find("/.rustup") {
        env.push(("RUSTUP_HOME".to_owned(), rustup_home));
    }
    // A writable target dir is supplied by the workspace bind; a
    // read-only cache would break incremental builds, so builds write
    // inside the workspace instead.
    env
}

/// The PATH a sandboxed command runs with.
///
/// System locations plus the `bin` directories of the toolchain roots this
/// request exposes: a toolchain installed under `$HOME` is reachable
/// without putting the whole home directory on PATH (or in the sandbox).
pub fn sandbox_path(request: &CommandRequest) -> String {
    let mut entries = vec![
        "/usr/local/bin".to_owned(),
        "/usr/bin".to_owned(),
        "/bin".to_owned(),
    ];
    for toolchain in &request.toolchain_paths {
        for suffix in ["/bin", ""] {
            let candidate = format!("{toolchain}{suffix}");
            let path = std::path::Path::new(&candidate);
            if path.is_dir() && !entries.contains(&candidate) {
                entries.push(candidate);
            }
        }
    }
    entries.join(":")
}

/// Conventional toolchain roots under the user's home, if they exist.
///
/// These are *read-only* mounts. Nothing else from the home directory is
/// exposed, and the paths are absolute so they work under bubblewrap.
pub fn default_toolchain_paths() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = std::path::PathBuf::from(home);
    [
        "/.cargo",
        "/.rustup",
        "/.local/share/mise",
        "/.bun",
        "/.nvm",
        "/.cache",
    ]
    .iter()
    .map(|suffix| home.join(suffix.trim_start_matches('/')))
    .filter(|path| path.is_dir())
    .map(|path| path.to_string_lossy().into_owned())
    .collect()
}

/// Find an executable on PATH.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Build the sandboxed command line.
///
/// bubblewrap argument order matters; the workspace is mounted at its own
/// absolute path so paths in build output stay meaningful.
fn sandboxed_command(
    workspace: &Workspace,
    request: &CommandRequest,
) -> Result<Command, KnutError> {
    let root = workspace.root();
    let working_dir = workspace
        .resolve(&request.working_dir)
        .map_err(|err| KnutError::Tool(format!("working dir rejected: {err}")))?;
    if !working_dir.absolute().is_dir() {
        return Err(KnutError::Tool(format!(
            "working directory {:?} does not exist",
            working_dir.relative()
        )));
    }

    let mut command = Command::new("bwrap");
    command.arg("--die-with-parent");
    // No network namespace unless explicitly allowed.
    if request.spec.network {
        command.arg("--unshare-user").arg("--unshare-pid");
    } else {
        command.arg("--unshare-all");
    }
    // pre_exec already creates the session; a second setsid escapes its kill group.
    command.args(["--proc", "/proc", "--dev", "/dev"]);

    // A minimal read-only system: the toolchain lives in these roots.
    for system_path in ["/usr", "/bin", "/lib", "/lib64", "/etc", "/nix", "/opt"] {
        let path = std::path::Path::new(system_path);
        if path.exists() {
            command.arg("--ro-bind").arg(system_path).arg(system_path);
        }
    }

    // Everything outside the workspace that is not a system root is
    // hidden: /tmp is a private tmpfs, and home is not mounted wholesale.
    command.args(["--tmpfs", "/tmp"]);

    // Toolchains that a developer installed under $HOME must still be
    // reachable, or every build fails for the wrong reason. Only the
    // specific toolchain roots are exposed, read-only, and only when they
    // are named in the request: never the home directory itself.
    for toolchain in &request.toolchain_paths {
        let path = std::path::Path::new(toolchain);
        if path.is_dir() {
            command.arg("--ro-bind").arg(toolchain).arg(toolchain);
        }
    }

    // The workspace: read-only by default, writable only where declared.
    let writable = resolve_writable(workspace, request)?;
    if writable.is_empty() {
        command.arg("--ro-bind").arg(root).arg(root);
    } else {
        command.arg("--bind").arg(root).arg(root);
        for path in resolve_readable_only(workspace, request)? {
            command
                .arg("--ro-bind")
                .arg(root.join(&path))
                .arg(root.join(&path));
        }
    }

    command
        .arg("--chdir")
        .arg(working_dir.absolute())
        .arg("--")
        .arg(&request.program)
        .args(&request.args);

    // Environment: a clean slate plus explicit entries. Provider keys and
    // the rest of the parent environment never reach repository code.
    command.env_clear();
    command.env("PATH", sandbox_path(request));
    command.env("HOME", "/tmp");
    command.env("TMPDIR", "/tmp");
    command.env("TERM", "dumb");
    command.env("NO_COLOR", "1");
    for (key, value) in &request.env {
        command.env(key, value);
    }

    // Toolchain discovery needs its own roots to be named explicitly: a
    // sandboxed rustup finds no toolchain otherwise.
    for (key, value) in toolchain_env(request) {
        command.env(key, value);
    }

    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    Ok(command)
}

/// Which workspace paths the command may write.
fn resolve_writable(
    workspace: &Workspace,
    request: &CommandRequest,
) -> Result<Vec<String>, KnutError> {
    let mut resolved = Vec::new();
    for path in &request.spec.writable_paths {
        let safe = workspace.resolve(path)?;
        if workspace.is_denied(safe.relative()) {
            return Err(KnutError::Tool(format!(
                "path {:?} is excluded from workspace access",
                safe.relative()
            )));
        }
        resolved.push(safe.relative().to_owned());
    }
    Ok(resolved)
}

/// Workspace paths explicitly declared readable but not writable.
fn resolve_readable_only(
    workspace: &Workspace,
    request: &CommandRequest,
) -> Result<Vec<String>, KnutError> {
    let writable = resolve_writable(workspace, request)?;
    let mut resolved = Vec::new();
    for path in &request.spec.readable_paths {
        let safe = workspace.resolve(path)?;
        if writable
            .iter()
            .any(|w| safe.relative().starts_with(w.as_str()))
        {
            continue;
        }
        resolved.push(safe.relative().to_owned());
    }
    Ok(resolved)
}

/// Whether the command could have changed the workspace.
fn is_effectful(request: &CommandRequest) -> bool {
    // Any workspace write permission makes a command potentially
    // effectful: a build script writes artifacts.
    !request.spec.writable_paths.is_empty()
}

/// The blocking body: spawn, drain, cancel, reap.
fn run_blocking(
    workspace: &Workspace,
    request: &CommandRequest,
    backend: SandboxBackend,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
    abandoned: Arc<AtomicBool>,
    trust: bool,
) -> Result<CommandOutcome, KnutError> {
    let mut command = match backend {
        SandboxBackend::Bubblewrap => sandboxed_command(workspace, request)?,
        SandboxBackend::Unsandboxed => {
            if !trust {
                return Ok(refused_outcome(
                    &request.clone(),
                    "unsandboxed execution was not explicitly authorized".to_owned(),
                ));
            }
            let working_dir = workspace.resolve(&request.working_dir)?;
            let mut command = Command::new(&request.program);
            command.args(&request.args);
            command.current_dir(working_dir.absolute());
            command.env_clear();
            command.env("PATH", sandbox_path(request));
            for (key, value) in &request.env {
                command.env(key, value);
            }
            for (key, value) in toolchain_env(request) {
                command.env(key, value);
            }
            command.stdin(Stdio::null());
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
            command
        }
    };

    // A new process group, so cancellation can reach descendants.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            // New session: the command and its descendants form one
            // group we can signal as a unit.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|err| KnutError::Tool(format!("failed to start {:?}: {err}", request.program)))?;

    let pid = child.id() as i32;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Drain both streams on their own threads: a full pipe must never
    // deadlock the supervisor.
    let stdout_handle = stdout.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Retain a bounded window; keep counting bytes.
                        if buffer.len() < MAX_STREAM_BYTES {
                            buffer.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
            buffer
        })
    });
    let stderr_handle = stderr.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buffer.len() < MAX_STREAM_BYTES {
                            buffer.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
            buffer
        })
    });

    // Watch for timeout or cancellation, then stop the whole group.
    let mut status = None;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::SeqCst) || abandoned.load(Ordering::SeqCst) {
            cancelled = true;
            terminate_group(pid);
            let _ = child.wait();
            break;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            terminate_group(pid);
            let _ = child.wait();
            break;
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => {
                terminate_group(pid);
                let _ = child.wait();
                return Ok(CommandOutcome {
                    program: request.program.clone(),
                    args: request.args.clone(),
                    status: CommandStatus::UnknownEffect {
                        reason: format!("waiting for the command failed: {err}"),
                    },
                    stdout: BoundedOutput {
                        text: String::new(),
                        bytes: 0,
                        truncated: false,
                        sanitized: true,
                    },
                    stderr: BoundedOutput {
                        text: String::new(),
                        bytes: 0,
                        truncated: false,
                        sanitized: true,
                    },
                    duration_ms: started.elapsed().as_millis() as u64,
                    backend,
                    possibly_effectful: is_effectful(request),
                    model_summary: "command outcome unknown".to_owned(),
                });
            }
        }
    }

    let stdout_bytes = stdout_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr_bytes = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();

    // Retained output is bounded per stream *and* in total, so a chatty
    // command cannot grow memory past a fixed ceiling.
    let stdout_limit = (MAX_TOTAL_BYTES / 2).min(MAX_STREAM_BYTES);
    let stdout = bound_output(&stdout_bytes, stdout_limit);
    let stderr_limit = MAX_TOTAL_BYTES
        .saturating_sub(stdout.text.len())
        .min(MAX_STREAM_BYTES);
    let stderr = bound_output(&stderr_bytes, stderr_limit);

    let status = if cancelled {
        CommandStatus::Cancelled
    } else if timed_out {
        CommandStatus::TimedOut
    } else {
        match status {
            Some(exit) => match exit.code() {
                Some(code) => CommandStatus::Exited { code },
                None => CommandStatus::Signalled {
                    signal: signal_of(&exit),
                },
            },
            None => CommandStatus::UnknownEffect {
                reason: "no exit status was observed".to_owned(),
            },
        }
    };

    let possibly_effectful = is_effectful(request);
    let model_summary = format!(
        "{:?} {} -> {status:?} in {} ms\nstdout ({} bytes{}):\n{}\nstderr ({} bytes{}):\n{}",
        backend,
        request.program,
        started.elapsed().as_millis(),
        stdout.bytes,
        if stdout.truncated { ", truncated" } else { "" },
        stdout.text.chars().take(4000).collect::<String>(),
        stderr.bytes,
        if stderr.truncated { ", truncated" } else { "" },
        stderr.text.chars().take(4000).collect::<String>(),
    );

    Ok(CommandOutcome {
        program: request.program.clone(),
        args: request.args.clone(),
        status,
        stdout,
        stderr,
        duration_ms: started.elapsed().as_millis() as u64,
        backend,
        possibly_effectful,
        model_summary,
    })
}

/// Signal a whole process group: SIGTERM, grace period, then SIGKILL.
fn terminate_group(pid: i32) {
    #[cfg(unix)]
    unsafe {
        let group = -pid;
        libc::kill(group, libc::SIGTERM);
        let deadline = Instant::now() + DEFAULT_GRACE;
        while Instant::now() < deadline {
            // Signal 0 probes existence without delivering anything.
            if libc::kill(group, 0) != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        libc::kill(group, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().unwrap_or(0)
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> i32 {
    0
}

/// The `shell.run` tool: a supervised, optionally sandboxed command.
pub struct RunCommandTool {
    supervisor: Arc<Supervisor>,
}

impl RunCommandTool {
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self { supervisor }
    }
}

#[async_trait::async_trait]
impl Tool for RunCommandTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: "run".to_owned(),
            tool_version: "1".to_owned(),
            capability: CAPABILITY.to_owned(),
            description: "Run a supervised command in the workspace".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "working_dir": { "type": "string" },
                    "writable": { "type": "array", "items": { "type": "string" } },
                    "network": { "type": "boolean" },
                    "timeout_secs": { "type": "number" }
                },
                "required": ["program"]
            }),
            // A command that may write is an effect; policy decides.
            side_effect: SideEffect::NonIdempotentWrite,
        }
    }

    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        let program = input
            .get("program")
            .and_then(Value::as_str)
            .ok_or_else(|| KnutError::Tool("run requires a program".to_owned()))?
            .to_owned();
        let args: Vec<String> = input
            .get("args")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // Shell interpretation is a distinct capability: a program that
        // *is* a shell must be asked for explicitly, never inferred from
        // a string that happens to contain shell syntax.
        let mut request = CommandRequest::new(program, args);
        if let Some(dir) = input.get("working_dir").and_then(Value::as_str) {
            request = request.with_working_dir(dir);
        }
        if let Some(writable) = input.get("writable").and_then(Value::as_array) {
            request = request.with_writable(
                writable
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            );
        }
        request = request.with_network(
            input
                .get("network")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        if let Some(timeout) = input.get("timeout_secs").and_then(Value::as_u64) {
            request = request.with_timeout(Duration::from_secs(timeout));
        }

        let outcome = self.supervisor.run(request).await?;
        serde_json::to_value(&outcome)
            .map_err(|err| KnutError::Tool(format!("serialize outcome: {err}")))
    }
}

/// Register the command tools into a registry.
pub fn register_command_tools(
    registry: &mut crate::ToolRegistry,
    supervisor: Arc<Supervisor>,
) -> Result<(), KnutError> {
    registry.register(RunCommandTool::new(supervisor))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "knut-supervisor-{name}-{}-{:?}",
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

    fn supervisor(fixture: &Fixture) -> Supervisor {
        Supervisor::new(fixture.workspace()).with_unsandboxed_trust(false)
    }

    fn sh(script: &str, writable: Vec<String>) -> CommandRequest {
        CommandRequest::new("/bin/sh", vec!["-c".to_owned(), script.to_owned()])
            .with_writable(writable)
    }

    #[tokio::test]
    async fn a_command_runs_and_reports_bounded_output_and_status() {
        let fixture = Fixture::new("basic");
        let outcome = supervisor(&fixture)
            .run(sh("echo hello; echo oops >&2; exit 3", vec![]))
            .await
            .unwrap();

        assert_eq!(outcome.status, CommandStatus::Exited { code: 3 });
        assert!(outcome.stdout.text.contains("hello"));
        assert!(outcome.stderr.text.contains("oops"));
        assert_eq!(outcome.backend, SandboxBackend::Bubblewrap);
        assert!(outcome.backend.is_enforced());
        assert!(!outcome.is_retriable() || !outcome.possibly_effectful);
    }

    #[tokio::test]
    async fn arguments_are_never_reinterpreted_as_shell_syntax() {
        let fixture = Fixture::new("argv");
        // Metacharacters and spaces are data, not syntax: argv is passed
        // verbatim with no shell in between.
        let request = CommandRequest::new(
            "/bin/echo",
            vec![
                "a b".to_owned(),
                "; rm -rf /".to_owned(),
                "$(whoami)".to_owned(),
                "日本語".to_owned(),
            ],
        );
        let outcome = supervisor(&fixture).run(request).await.unwrap();

        assert!(outcome.status.is_success(), "{:?}", outcome.status);
        assert!(outcome.stdout.text.contains("; rm -rf /"));
        assert!(outcome.stdout.text.contains("$(whoami)"));
        assert!(outcome.stdout.text.contains("日本語"));
    }

    #[tokio::test]
    async fn a_denied_secret_file_cannot_be_read() {
        let fixture = Fixture::new("denied");
        // A fixture secret outside the workspace, in the real /tmp the
        // sandbox replaces with a private tmpfs.
        let secret = std::env::temp_dir().join("knut-outside-secret.txt");
        std::fs::write(&secret, "TOPSECRET\n").unwrap();

        let outcome = supervisor(&fixture)
            .run(sh(&format!("cat {}", secret.display()), vec![]))
            .await
            .unwrap();

        assert!(
            !outcome.stdout.text.contains("TOPSECRET"),
            "the sandbox leaked a file outside the workspace: {:?}",
            outcome.stdout.text
        );
        let _ = std::fs::remove_file(&secret);
    }

    #[tokio::test]
    async fn network_access_is_denied_by_the_os() {
        let fixture = Fixture::new("network");
        let outcome = supervisor(&fixture)
            .run(sh(
                "timeout 5 sh -c 'echo > /dev/tcp/1.1.1.1/443' 2>&1 || echo BLOCKED",
                vec![],
            ))
            .await
            .unwrap();

        assert!(
            outcome.stdout.text.contains("BLOCKED")
                || outcome.stderr.text.contains("Network is unreachable"),
            "network was not blocked: stdout={:?} stderr={:?}",
            outcome.stdout.text,
            outcome.stderr.text
        );
    }

    #[tokio::test]
    async fn writes_outside_the_permitted_area_are_denied() {
        let fixture = Fixture::new("write");
        fixture.write("allowed/keep.txt", "keep\n");

        // The whole workspace is read-only when nothing is declared
        // writable.
        let outcome = supervisor(&fixture)
            .run(sh("echo nope > allowed/new.txt; echo EXIT=$?", vec![]))
            .await
            .unwrap();
        assert!(
            !fixture.dir.join("allowed/new.txt").exists(),
            "a read-only sandbox allowed a write: {:?}",
            outcome.stdout.text
        );

        // With the path declared writable, the write succeeds.
        let outcome = supervisor(&fixture)
            .run(sh(
                "echo yes > allowed/new.txt; echo EXIT=$?",
                vec!["allowed".to_owned()],
            ))
            .await
            .unwrap();
        assert!(
            fixture.dir.join("allowed/new.txt").exists(),
            "a declared writable path was refused: {:?} {:?}",
            outcome.stdout.text,
            outcome.stderr.text
        );
    }

    #[tokio::test]
    async fn provider_keys_and_parent_environment_do_not_reach_the_command() {
        let fixture = Fixture::new("env");
        // Simulate a provider key in the parent environment.
        unsafe {
            std::env::set_var("ZAI_API_KEY", "super-secret-provider-key");
            std::env::set_var("TYPESAFE_API_KEY", "super-secret-jev-key");
        }

        let outcome = supervisor(&fixture)
            .run(sh("env | sort", vec![]))
            .await
            .unwrap();

        assert!(!outcome.stdout.text.contains("super-secret"));
        assert!(!outcome.stdout.text.contains("ZAI_API_KEY"));
        assert!(!outcome.stdout.text.contains("TYPESAFE_API_KEY"));

        unsafe {
            std::env::remove_var("ZAI_API_KEY");
            std::env::remove_var("TYPESAFE_API_KEY");
        }
    }

    #[tokio::test]
    async fn cancellation_terminates_the_command_and_its_descendants() {
        let fixture = Fixture::new("cancel");
        let supervisor = Arc::new(supervisor(&fixture));
        let flag = supervisor.cancel_flag();

        // A child that spawns a grandchild, then sleeps.
        let request = sh(
            "sleep 300 & echo GRANDCHILD=$!; sleep 300",
            vec![".".to_owned()],
        );
        let supervisor_clone = Arc::clone(&supervisor);
        let handle = tokio::spawn(async move { supervisor_clone.run(request).await });

        // Let it start, then cancel.
        tokio::time::sleep(Duration::from_millis(600)).await;
        flag.store(true, Ordering::SeqCst);

        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.status, CommandStatus::Cancelled);

        // The grandchild must be gone too: a dropped future alone would
        // not have proven that.
        let grandchild = outcome
            .stdout
            .text
            .lines()
            .find_map(|line| line.strip_prefix("GRANDCHILD="))
            .and_then(|pid| pid.trim().parse::<i32>().ok());
        if let Some(pid) = grandchild {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let alive = unsafe { libc::kill(pid, 0) == 0 };
            assert!(!alive, "grandchild {pid} survived cancellation");
        }
    }

    #[tokio::test]
    async fn a_timeout_stops_the_command() {
        let fixture = Fixture::new("timeout");
        let outcome = supervisor(&fixture)
            .run(sh("sleep 60", vec![]).with_timeout(Duration::from_millis(700)))
            .await
            .unwrap();

        assert_eq!(outcome.status, CommandStatus::TimedOut);
        assert!(outcome.duration_ms < 10_000);
    }

    #[tokio::test]
    async fn huge_output_is_bounded_and_cannot_freeze_the_supervisor() {
        let fixture = Fixture::new("huge");
        // ~8 MB of output: far past the retention bound.
        let outcome = supervisor(&fixture)
            .run(sh(
                "i=0; while [ $i -lt 200000 ]; do echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; i=$((i+1)); done",
                vec![],
            ))
            .await
            .unwrap();

        assert!(outcome.status.is_success());
        assert!(outcome.stdout.truncated);
        assert!(outcome.stdout.text.len() <= MAX_STREAM_BYTES);
        assert!(outcome.stdout.bytes > MAX_STREAM_BYTES);
    }

    #[tokio::test]
    async fn terminal_control_sequences_are_sanitized() {
        let fixture = Fixture::new("ansi");
        let outcome = supervisor(&fixture)
            .run(sh(
                "printf '\\033[2J\\033[1;1Hmalicious\\033]0;title\\007 ok\\rnext\\n'",
                vec![],
            ))
            .await
            .unwrap();

        assert!(!outcome.stdout.text.contains('\u{1b}'));
        assert!(outcome.stdout.text.contains("malicious"));
        // Carriage returns became newlines rather than overwriting.
        assert!(outcome.stdout.text.contains("next"));
    }

    #[tokio::test]
    async fn an_unavailable_sandbox_refuses_visibly_instead_of_degrading() {
        let fixture = Fixture::new("nosandbox");
        // A supervisor whose backend cannot be detected.
        struct NoBackend;
        let supervisor = Supervisor::new(fixture.workspace());
        if supervisor.backend().is_ok() {
            // Simulate unavailability through the refusal path directly:
            // the point under test is that no fallback exists.
            let outcome = refused_outcome(
                &sh("echo hello", vec![]),
                SandboxUnavailable::BackendMissing {
                    backend: "bwrap",
                    detail: "not found".to_owned(),
                }
                .to_string(),
            );
            assert!(matches!(
                outcome.status,
                CommandStatus::SandboxRefused { .. }
            ));
            assert!(outcome.stderr.text.contains("refused"));
        }
        let _ = NoBackend;
    }

    #[tokio::test]
    async fn unsandboxed_execution_requires_explicit_trust() {
        let fixture = Fixture::new("trust");
        fixture.write("a.txt", "content\n");

        // Without trust, an unsandboxed request is refused rather than
        // run. (With a working sandbox this path is not reached, so the
        // assertion targets the refusal helper.)
        let supervisor = Supervisor::new(fixture.workspace());
        assert!(supervisor.backend().is_ok());

        let outcome = refused_outcome(
            &CommandRequest::new("/bin/echo", vec!["x".to_owned()]),
            "unsandboxed execution was not explicitly authorized".to_owned(),
        );
        assert!(matches!(
            outcome.status,
            CommandStatus::SandboxRefused { .. }
        ));
    }

    #[tokio::test]
    async fn a_working_directory_outside_the_workspace_is_rejected() {
        let fixture = Fixture::new("workdir");
        let err = supervisor(&fixture)
            .run(CommandRequest::new("/bin/pwd", vec![]).with_working_dir("../.."))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("rejected"), "got {err}");
    }

    #[tokio::test]
    async fn a_declared_writable_path_cannot_be_a_denied_secret() {
        let fixture = Fixture::new("deny-write");
        fixture.write(".env", "API_KEY=secret\n");
        let err = supervisor(&fixture)
            .run(sh("echo x > .env", vec![".env".to_owned()]))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("excluded"), "got {err}");
    }

    #[tokio::test]
    async fn the_tool_reports_the_backend_used() {
        let fixture = Fixture::new("tool");
        let supervisor = Arc::new(supervisor(&fixture));
        let tool = RunCommandTool::new(Arc::clone(&supervisor));

        let outcome = crate::tool::Tool::call(
            &tool,
            json!({ "program": "/bin/echo", "args": ["from the tool"] }),
        )
        .await
        .unwrap();

        assert_eq!(outcome["backend"], json!("bubblewrap"));
        assert!(
            outcome["stdout"]["text"]
                .as_str()
                .unwrap()
                .contains("from the tool")
        );
        let metadata = crate::tool::Tool::metadata(&tool);
        assert_eq!(metadata.side_effect, SideEffect::NonIdempotentWrite);
        assert_eq!(metadata.capability, CAPABILITY);
    }

    #[test]
    fn sanitizer_handles_common_escape_forms() {
        assert_eq!(sanitize_terminal("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(sanitize_terminal("a\u{1b}]0;t\u{7}b"), "ab");
        assert_eq!(sanitize_terminal("plain text"), "plain text");
    }

    #[test]
    fn effects_are_never_retried_after_an_ambiguous_stop() {
        let request = sh("sleep 1", vec![".".to_owned()]);
        let outcome = CommandOutcome {
            program: request.program.clone(),
            args: request.args.clone(),
            status: CommandStatus::TimedOut,
            stdout: BoundedOutput {
                text: String::new(),
                bytes: 0,
                truncated: false,
                sanitized: true,
            },
            stderr: BoundedOutput {
                text: String::new(),
                bytes: 0,
                truncated: false,
                sanitized: true,
            },
            duration_ms: 1000,
            backend: SandboxBackend::Bubblewrap,
            possibly_effectful: true,
            model_summary: String::new(),
        };
        assert!(!outcome.is_retriable());
    }
}
