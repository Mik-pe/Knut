#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteCommand {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

pub fn command_catalog() -> Vec<PaletteCommand> {
    vec![
        PaletteCommand {
            id: "submit",
            title: "Submit prompt",
            description: "Send the composed request as a new task",
        },
        PaletteCommand {
            id: "steer",
            title: "Steer task",
            description: "Redirect the running task (bumps its revision)",
        },
        PaletteCommand {
            id: "queue",
            title: "Queue next task",
            description: "Keep this draft as a separate task after the active one",
        },
        PaletteCommand {
            id: "pause",
            title: "Pause task",
            description: "Stop dispatching new work",
        },
        PaletteCommand {
            id: "resume",
            title: "Resume task",
            description: "Continue after a pause",
        },
        PaletteCommand {
            id: "cancel",
            title: "Cancel task",
            description: "Stop the running task and its tool processes",
        },
        PaletteCommand {
            id: "verify",
            title: "Run checks",
            description: "Run the workspace's build/test/lint checks",
        },
        PaletteCommand {
            id: "settings",
            title: "Settings",
            description: "Save your connection and model without launch arguments · F2",
        },
        PaletteCommand {
            id: "account",
            title: "ChatGPT account",
            description: "Continue with ChatGPT, choose a model or sign out",
        },
        PaletteCommand {
            id: "usage",
            title: "Manage usage",
            description: "Open ChatGPT plan usage in the browser",
        },
        PaletteCommand {
            id: "doctor",
            title: "Diagnose setup",
            description: "Report configured providers without calling them",
        },
        PaletteCommand {
            id: "motion",
            title: "Toggle animations",
            description: "Switch between the animated knot and reduced motion",
        },
        PaletteCommand {
            id: "help",
            title: "Show help",
            description: "Keyboard reference",
        },
        PaletteCommand {
            id: "review",
            title: "Review change",
            description: "Open the diff and evidence review workspace",
        },
        PaletteCommand {
            id: "jobs",
            title: "Show jobs",
            description: "Active tools and queued requests · Ctrl+O",
        },
        PaletteCommand {
            id: "decisions",
            title: "Show decisions",
            description: "Session evidence and routing · Ctrl+B",
        },
        PaletteCommand {
            id: "model",
            title: "Switch model",
            description: "Choose an available ChatGPT model",
        },
    ]
}

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

fn fuzzy_match(query: &str, haystack: &str) -> bool {
    let haystack = haystack.to_lowercase();
    let mut chars = haystack.chars();
    query
        .chars()
        .all(|wanted| chars.any(|candidate| candidate == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_subsequences_and_preserves_catalog_order() {
        let catalog = command_catalog();
        assert_eq!(filter_catalog(" "), catalog);
        assert!(filter_catalog("zzzz").is_empty());
        assert!(
            filter_catalog("CNL")
                .iter()
                .any(|command| command.id == "cancel")
        );
        let results = filter_catalog("task");
        let positions: Vec<_> = results
            .iter()
            .map(|command| {
                catalog
                    .iter()
                    .position(|entry| entry.id == command.id)
                    .unwrap()
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
