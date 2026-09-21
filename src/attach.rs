//! Composer attachments and the command palette (issue #28).
//!
//! `@file` mentions resolve against the *workspace tools*, so what a
//! mention attaches is exactly what a read would return — bounded, with
//! ignore rules and secret exclusions applied. A mention is never consent
//! to send anything to a provider: the user still approves the task, and
//! the egress policy is unchanged by mentioning a file.
//!
//! The palette lists only commands that exist. A command with no
//! implementation is reported as unavailable rather than succeeding
//! silently.

use serde_json::Value;

use crate::workspace::Workspace;

/// Maximum attachments in one prompt.
pub const MAX_ATTACHMENTS: usize = 8;
/// Maximum characters of one attachment's excerpt.
pub const MAX_EXCERPT_CHARS: usize = 4_000;

/// One resolved file attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Workspace-relative path.
    pub path: String,
    /// Inclusive line range, when the mention asked for one.
    pub range: Option<(usize, usize)>,
    pub content_hash: String,
    pub excerpt: String,
    pub excerpt_truncated: bool,
}

/// Why a mention could not be attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// The path does not resolve inside the workspace.
    Unsafe { path: String, reason: String },
    /// The path is excluded (secret/ignored).
    Excluded { path: String },
    /// The file could not be read.
    Unreadable { path: String, reason: String },
    /// Too many attachments already.
    TooMany,
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::Unsafe { path, reason } => {
                write!(f, "{path:?} is not a workspace path: {reason}")
            }
            AttachError::Excluded { path } => {
                write!(f, "{path:?} is excluded from workspace reads")
            }
            AttachError::Unreadable { path, reason } => write!(f, "{path:?}: {reason}"),
            AttachError::TooMany => write!(f, "at most {MAX_ATTACHMENTS} attachments per prompt"),
        }
    }
}

/// Resolve one mention into an attachment.
///
/// Accepts `path`, `path:12` or `path:12-34`.
pub fn resolve_mention(workspace: &Workspace, mention: &str) -> Result<Attachment, AttachError> {
    let mention = mention.trim().trim_start_matches('@');
    if mention.is_empty() {
        return Err(AttachError::Unsafe {
            path: mention.to_owned(),
            reason: "empty attachment".to_owned(),
        });
    }

    // A trailing `:12` or `:12-34` selects a line range.
    let (path, range) = split_range(mention);

    let safe = workspace.resolve(path).map_err(|err| AttachError::Unsafe {
        path: path.to_owned(),
        reason: err.to_string(),
    })?;
    if workspace.is_denied(safe.relative()) {
        return Err(AttachError::Excluded {
            path: safe.relative().to_owned(),
        });
    }

    let bytes = workspace
        .read_bytes(path)
        .map_err(|err| AttachError::Unreadable {
            path: path.to_owned(),
            reason: err.to_string(),
        })?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();

    let excerpt_raw = match range {
        Some((start, end)) => lines
            .iter()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start) + 1)
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
        None => lines.join("\n"),
    };
    let excerpt_truncated = excerpt_raw.chars().count() > MAX_EXCERPT_CHARS;
    let excerpt: String = excerpt_raw.chars().take(MAX_EXCERPT_CHARS).collect();

    Ok(Attachment {
        path: safe.relative().to_owned(),
        range,
        content_hash: crate::workspace::content_hash(&bytes),
        excerpt,
        excerpt_truncated,
    })
}

/// Split `path:12-34` into a path and its range.
fn split_range(mention: &str) -> (&str, Option<(usize, usize)>) {
    let Some(colon) = mention.rfind(':') else {
        return (mention, None);
    };
    let suffix = &mention[colon + 1..];
    if let Some((start, end)) = suffix.split_once('-')
        && let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>())
    {
        return (&mention[..colon], Some((start, end)));
    }
    if let Ok(line) = suffix.parse::<usize>() {
        return (&mention[..colon], Some((line, line)));
    }
    (mention, None)
}

/// Resolve every mention found in a prompt.
///
/// Returns the resolved attachments and the errors, both bounded: a
/// mention that cannot be attached is reported, never silently dropped.
pub fn resolve_mentions(
    workspace: &Workspace,
    mentions: &[String],
) -> (Vec<Attachment>, Vec<AttachError>) {
    let mut attachments = Vec::new();
    let mut errors = Vec::new();
    for mention in mentions.iter().take(MAX_ATTACHMENTS) {
        match resolve_mention(workspace, mention) {
            Ok(attachment) => attachments.push(attachment),
            Err(err) => errors.push(err),
        }
    }
    if mentions.len() > MAX_ATTACHMENTS {
        errors.push(AttachError::TooMany);
    }
    (attachments, errors)
}

/// Extract `@path` mentions from composer text.
///
/// Deliberately simple and documented: a mention is `@` followed by
/// non-space characters. Quoting and escaping are not special-cased yet,
/// which is stated rather than implied.
pub fn extract_mentions(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    for token in text.split_whitespace() {
        if let Some(mention) = token.strip_prefix('@')
            && !mention.is_empty()
            && !mentions.contains(&mention.to_owned())
        {
            mentions.push(mention.to_owned());
        }
    }
    mentions
}

/// What a palette command does, or why it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAvailability {
    /// Implemented and usable now.
    Available,
    /// Known but not implemented yet: shown as unavailable, never as a
    /// pretend-success stub.
    Unavailable(&'static str),
}

/// One palette entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteCommand {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub availability: CommandAvailability,
}

impl PaletteCommand {
    pub fn is_available(&self) -> bool {
        matches!(self.availability, CommandAvailability::Available)
    }
}

/// The command catalog.
///
/// Every entry states whether it exists. Entries that are not implemented
/// say so, so the palette never claims a capability the binary lacks.
pub fn command_catalog() -> Vec<PaletteCommand> {
    vec![
        PaletteCommand {
            id: "submit",
            title: "Submit prompt",
            description: "Send the composed request as a new task",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "steer",
            title: "Steer task",
            description: "Redirect the running task (bumps its revision)",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "pause",
            title: "Pause task",
            description: "Stop dispatching new work",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "resume",
            title: "Resume task",
            description: "Continue after a pause",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "cancel",
            title: "Cancel task",
            description: "Stop the running task and its tool processes",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "verify",
            title: "Run checks",
            description: "Run the workspace's build/test/lint checks",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "attach",
            title: "Attach file",
            description: "Add a workspace file or range as context (@path)",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "doctor",
            title: "Diagnose setup",
            description: "Report configured providers without calling them",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "help",
            title: "Show help",
            description: "Keyboard reference",
            availability: CommandAvailability::Available,
        },
        PaletteCommand {
            id: "review",
            title: "Review change",
            description: "Open the diff and evidence review workspace",
            availability: CommandAvailability::Unavailable("arrives with #29"),
        },
        PaletteCommand {
            id: "model",
            title: "Switch model",
            description: "Change the reasoner or its effort",
            availability: CommandAvailability::Unavailable(
                "model selection is env-based until #41 adds provider catalogs",
            ),
        },
        PaletteCommand {
            id: "sessions",
            title: "Resume session",
            description: "List and resume previous sessions",
            availability: CommandAvailability::Unavailable("session persistence arrives with #31"),
        },
        PaletteCommand {
            id: "subagents",
            title: "Spawn subagent",
            description: "Run bounded parallel work in an isolated workspace",
            availability: CommandAvailability::Unavailable("arrives with #38"),
        },
    ]
}

/// Fuzzy-filter the catalog: every query character must appear in order.
pub fn filter_catalog(query: &str) -> Vec<PaletteCommand> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return command_catalog();
    }
    command_catalog()
        .into_iter()
        .filter(|command| fuzzy_match(&query, &format!("{} {}", command.title, command.id)))
        .collect()
}

/// Subsequence match, case-insensitive.
fn fuzzy_match(query: &str, haystack: &str) -> bool {
    let haystack = haystack.to_lowercase();
    let mut chars = haystack.chars();
    query
        .chars()
        .all(|wanted| chars.any(|candidate| candidate == wanted))
}

/// Build the structured context payload for a task with attachments.
pub fn attachment_payload(attachments: &[Attachment]) -> Value {
    let files: Vec<Value> = attachments
        .iter()
        .map(|attachment| {
            serde_json::json!({
                "path": attachment.path,
                "range": attachment.range.map(|(start, end)| serde_json::json!({
                    "start": start,
                    "end": end,
                })),
                "content_hash": attachment.content_hash,
                "truncated": attachment.excerpt_truncated,
                "content": attachment.excerpt,
            })
        })
        .collect();
    serde_json::json!({ "attached_files": files })
}

/// A summary of what will be attached, shown before submission.
pub fn attachment_summary(attachments: &[Attachment], errors: &[AttachError]) -> String {
    let mut lines = Vec::new();
    for attachment in attachments {
        let range = match attachment.range {
            Some((start, end)) if start == end => format!(":{start}"),
            Some((start, end)) => format!(":{start}-{end}"),
            None => String::new(),
        };
        let truncated = if attachment.excerpt_truncated {
            " (truncated)"
        } else {
            ""
        };
        lines.push(format!(
            "  {}{range} [{} chars]{truncated}",
            attachment.path,
            attachment.excerpt.chars().count()
        ));
    }
    for error in errors {
        lines.push(format!("  not attached: {error}"));
    }
    if lines.is_empty() {
        return "no attachments".to_owned();
    }
    lines.join("\n")
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
                "knut-attach-{name}-{}-{:?}",
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
    fn mentions_resolve_to_real_workspace_content() {
        let fixture = Fixture::new("resolve");
        fixture.write("src/lib.rs", "fn main() {}\nfn helper() {}\n");
        let workspace = fixture.workspace();

        let attachment = resolve_mention(&workspace, "@src/lib.rs").unwrap();
        assert_eq!(attachment.path, "src/lib.rs");
        assert!(attachment.excerpt.contains("fn main"));
        assert!(attachment.content_hash.starts_with("fnv1a:"));

        // A range attaches only those lines, with the range reported.
        let ranged = resolve_mention(&workspace, "@src/lib.rs:2-2").unwrap();
        assert_eq!(ranged.range, Some((2, 2)));
        assert_eq!(ranged.excerpt, "fn helper() {}");
        assert!(!ranged.excerpt.contains("fn main"));
    }

    #[test]
    fn mentions_cannot_reach_outside_or_into_secrets() {
        let fixture = Fixture::new("denied");
        fixture.write("ok.txt", "ok\n");
        fixture.write(".env", "SECRET=1\n");
        let workspace = fixture.workspace();

        assert!(matches!(
            resolve_mention(&workspace, "@../outside.txt"),
            Err(AttachError::Unsafe { .. })
        ));
        assert!(matches!(
            resolve_mention(&workspace, "@/etc/passwd"),
            Err(AttachError::Unsafe { .. })
        ));
        // The deny list applies to attachments exactly as to reads.
        assert!(matches!(
            resolve_mention(&workspace, "@.env"),
            Err(AttachError::Excluded { .. })
        ));
    }

    #[test]
    fn a_large_file_is_attached_truncated_and_says_so() {
        let fixture = Fixture::new("large");
        fixture.write("big.txt", &"line\n".repeat(5_000));
        let workspace = fixture.workspace();

        let attachment = resolve_mention(&workspace, "@big.txt").unwrap();
        assert!(attachment.excerpt_truncated);
        assert!(attachment.excerpt.chars().count() <= MAX_EXCERPT_CHARS);
        assert!(attachment_summary(&[attachment], &[]).contains("truncated"));
    }

    #[test]
    fn mentions_are_extracted_from_prompt_text() {
        let mentions = extract_mentions("fix @src/lib.rs and check @tests/x.rs:10-20 please");
        assert_eq!(
            mentions,
            vec!["src/lib.rs".to_owned(), "tests/x.rs:10-20".to_owned()]
        );

        // Duplicates collapse; an email-like token is not a mention of a
        // path unless it starts the token.
        let mentions = extract_mentions("see @a.txt and @a.txt");
        assert_eq!(mentions, vec!["a.txt".to_owned()]);
        assert!(extract_mentions("user@example.com").is_empty());
    }

    #[test]
    fn attachments_are_bounded_and_errors_are_reported() {
        let fixture = Fixture::new("bounded");
        fixture.write("a.txt", "a\n");
        let workspace = fixture.workspace();

        let many: Vec<String> = (0..MAX_ATTACHMENTS + 3)
            .map(|i| format!("a.txt?{i}"))
            .collect();
        let (attachments, errors) = resolve_mentions(&workspace, &many);
        // Only the accepted ones resolve; the rest are reported, not
        // silently ignored.
        assert!(attachments.len() <= MAX_ATTACHMENTS);
        assert!(!errors.is_empty());
    }

    #[test]
    fn the_payload_names_paths_and_hashes_without_granting_egress() {
        let fixture = Fixture::new("payload");
        fixture.write("src/lib.rs", "fn main() {}\n");
        let workspace = fixture.workspace();

        let attachment = resolve_mention(&workspace, "@src/lib.rs").unwrap();
        let payload = attachment_payload(&[attachment]);
        let files = payload["attached_files"].as_array().unwrap();
        assert_eq!(files[0]["path"], serde_json::json!("src/lib.rs"));
        assert!(
            files[0]["content_hash"]
                .as_str()
                .unwrap()
                .starts_with("fnv1a:")
        );

        // The payload is local context: it names nothing about providers
        // or destinations, which remain an explicit egress decision.
        let text = payload.to_string();
        assert!(!text.contains("http"));
    }

    #[test]
    fn the_palette_only_offers_commands_that_exist() {
        let catalog = command_catalog();
        assert!(!catalog.is_empty());

        let available: Vec<&str> = catalog
            .iter()
            .filter(|command| command.is_available())
            .map(|command| command.id)
            .collect();
        assert!(available.contains(&"submit"));
        assert!(available.contains(&"steer"));
        assert!(available.contains(&"cancel"));
        assert!(available.contains(&"verify"));

        // Unimplemented commands are explicitly unavailable, with a
        // reason that names the issue.
        let unavailable: Vec<(&str, CommandAvailability)> = catalog
            .iter()
            .filter(|command| !command.is_available())
            .map(|command| (command.id, command.availability))
            .collect();
        assert!(!unavailable.is_empty());
        for (id, availability) in unavailable {
            match availability {
                CommandAvailability::Unavailable(reason) => {
                    assert!(
                        reason.contains('#'),
                        "{id} is unavailable without naming where it arrives"
                    );
                }
                CommandAvailability::Available => unreachable!(),
            }
        }
    }

    #[test]
    fn palette_search_is_fuzzy_and_ordered_by_the_catalog() {
        // Every query character must appear in order.
        let results = filter_catalog("cnl");
        assert!(
            results.iter().any(|command| command.id == "cancel"),
            "fuzzy search missed cancel: {results:?}"
        );

        let results = filter_catalog("zzzz");
        assert!(results.is_empty());

        // An empty query lists everything.
        assert_eq!(filter_catalog("").len(), command_catalog().len());
    }

    #[test]
    fn no_palette_command_claims_a_hidden_or_fake_success() {
        // Every entry must be classified: there is no third state that
        // could succeed silently.
        for command in command_catalog() {
            match command.availability {
                CommandAvailability::Available | CommandAvailability::Unavailable(_) => {}
            }
            assert!(!command.title.is_empty());
            assert!(!command.description.is_empty());
        }
    }

    #[test]
    fn attachment_summary_lists_what_will_be_sent() {
        let fixture = Fixture::new("summary");
        fixture.write("a.txt", "one\ntwo\nthree\n");
        let workspace = fixture.workspace();

        let (attachments, errors) = resolve_mentions(
            &workspace,
            &["a.txt:1-2".to_owned(), "missing.txt".to_owned()],
        );
        let summary = attachment_summary(&attachments, &errors);

        assert!(summary.contains("a.txt:1-2"));
        assert!(summary.contains("not attached"));
        assert!(summary.contains("missing.txt"));
    }
}
