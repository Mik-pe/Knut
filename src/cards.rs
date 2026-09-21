//! Streaming action cards for the workbench (issue #28).
//!
//! A card is the UI view of one unit of work: a tool call, a plan node or
//! a model turn. Cards carry stable identifiers, an explicit lifecycle
//! state, elapsed time and expandable output, so a stream is readable
//! while it runs and distinguishable afterwards.
//!
//! States mirror what the engine actually reports. Nothing here invents
//! progress: a card only changes state when a session event says so, and
//! "succeeded" is never assumed from silence.

use serde_json::Value;

use crate::tree::NodeStatus;

/// Lifecycle of one action card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardState {
    Queued,
    Running,
    /// Waiting on the user (approval or a question).
    Waiting,
    Succeeded,
    Failed,
    /// Stopped by cancellation — distinct from failure.
    Cancelled,
    /// Exceeded its budget — distinct from failure.
    TimedOut,
}

impl CardState {
    /// Text marker, so state is legible without color.
    pub fn marker(self) -> &'static str {
        match self {
            CardState::Queued => "·",
            CardState::Running => ">",
            CardState::Waiting => "?",
            CardState::Succeeded => "+",
            CardState::Failed => "!",
            CardState::Cancelled => "x",
            CardState::TimedOut => "t",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CardState::Queued => "queued",
            CardState::Running => "running",
            CardState::Waiting => "waiting",
            CardState::Succeeded => "succeeded",
            CardState::Failed => "failed",
            CardState::Cancelled => "cancelled",
            CardState::TimedOut => "timed out",
        }
    }

    /// Whether the card is still active (spinner-worthy).
    pub fn is_active(self) -> bool {
        matches!(
            self,
            CardState::Queued | CardState::Running | CardState::Waiting
        )
    }

    /// Map a node status from the engine onto a card state.
    pub fn from_node_status(status: NodeStatus) -> Self {
        match status {
            NodeStatus::Pending => CardState::Queued,
            NodeStatus::Running => CardState::Running,
            NodeStatus::Succeeded => CardState::Succeeded,
            NodeStatus::Failed => CardState::Failed,
            NodeStatus::Blocked => CardState::Waiting,
        }
    }
}

/// One action card.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionCard {
    /// Stable identifier from the engine (node id or tool-call id).
    pub id: String,
    /// Short label: the tool or node name.
    pub title: String,
    pub state: CardState,
    /// Elapsed time so far, or the final duration.
    pub elapsed_ms: u64,
    /// One-line summary shown collapsed.
    pub summary: String,
    /// Full bounded output, shown when expanded.
    pub detail: String,
    pub expanded: bool,
}

/// Maximum characters kept per card's detail.
pub const MAX_CARD_DETAIL: usize = 20_000;
/// Maximum cards retained (older ones are dropped from the front).
pub const MAX_CARDS: usize = 500;

impl ActionCard {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            state: CardState::Queued,
            elapsed_ms: 0,
            summary: String::new(),
            detail: String::new(),
            expanded: false,
        }
    }

    /// The one-line rendering of this card.
    pub fn headline(&self) -> String {
        let timing = format!("{}ms", self.elapsed_ms);
        if self.summary.is_empty() {
            format!(
                "{} {} [{}] {}",
                self.state.marker(),
                self.title,
                self.state.label(),
                timing
            )
        } else {
            format!(
                "{} {} [{}] {} — {}",
                self.state.marker(),
                self.title,
                self.state.label(),
                timing,
                self.summary
            )
        }
    }

    /// Set the detail, bounded.
    pub fn set_detail(&mut self, detail: impl Into<String>) {
        let mut detail = detail.into();
        if detail.chars().count() > MAX_CARD_DETAIL {
            detail = detail.chars().take(MAX_CARD_DETAIL).collect();
            detail.push('…');
        }
        self.detail = detail;
    }
}

/// The card list for one session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardList {
    cards: Vec<ActionCard>,
}

impl CardList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cards(&self) -> &[ActionCard] {
        &self.cards
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// Find a card by its stable id.
    pub fn get(&self, id: &str) -> Option<&ActionCard> {
        self.cards.iter().find(|card| card.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut ActionCard> {
        self.cards.iter_mut().find(|card| card.id == id)
    }

    /// Start (or re-state) a card.
    pub fn start(&mut self, id: impl Into<String>, title: impl Into<String>) {
        let id = id.into();
        if let Some(card) = self.get_mut(&id) {
            card.state = CardState::Running;
            return;
        }
        let mut card = ActionCard::new(id, title);
        card.state = CardState::Running;
        self.push_card(card);
    }

    fn push_card(&mut self, card: ActionCard) {
        self.cards.push(card);
        while self.cards.len() > MAX_CARDS {
            self.cards.remove(0);
        }
    }

    /// Finish a card with a state, summary and detail.
    pub fn finish(
        &mut self,
        id: &str,
        state: CardState,
        summary: impl Into<String>,
        detail: impl Into<String>,
        elapsed_ms: u64,
    ) {
        match self.get_mut(id) {
            Some(card) => {
                card.state = state;
                card.summary = summary.into();
                card.set_detail(detail);
                card.elapsed_ms = elapsed_ms;
            }
            None => {
                // A result without a start still becomes a visible card:
                // silently dropping it would hide work that happened.
                let mut card = ActionCard::new(id.to_owned(), id.to_owned());
                card.state = state;
                card.summary = summary.into();
                card.set_detail(detail);
                card.elapsed_ms = elapsed_ms;
                self.push_card(card);
            }
        }
    }

    /// Record the elapsed time of a running card.
    pub fn tick(&mut self, id: &str, elapsed_ms: u64) {
        if let Some(card) = self.get_mut(id) {
            card.elapsed_ms = elapsed_ms;
        }
    }

    /// Toggle expansion of one card.
    pub fn toggle(&mut self, id: &str) {
        if let Some(card) = self.get_mut(id) {
            card.expanded = !card.expanded;
        }
    }

    /// Collapse every card (used when the user asks for a compact view).
    pub fn collapse_all(&mut self) {
        for card in &mut self.cards {
            card.expanded = false;
        }
    }

    /// Whether any card is still active.
    pub fn any_active(&self) -> bool {
        self.cards.iter().any(|card| card.state.is_active())
    }
}

/// Sanitize untrusted output for display: no ANSI/OSC/control sequences.
///
/// Reuses the supervisor's sanitizer so a build log and a card render the
/// same way.
pub fn sanitize_for_display(text: &str) -> String {
    crate::sandbox::sanitize_terminal(text)
}

/// One-line summary of a structured tool result.
pub fn summarize_value(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    match value {
        Value::String(text) => {
            let first_line = text.lines().next().unwrap_or_default();
            first_line.chars().take(120).collect()
        }
        other => {
            let text = other.to_string();
            text.chars().take(120).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cards_move_through_the_documented_states() {
        let mut cards = CardList::new();
        cards.start("node-1", "read src/lib.rs");
        assert_eq!(cards.get("node-1").unwrap().state, CardState::Running);

        cards.finish(
            "node-1",
            CardState::Succeeded,
            "read 42 lines",
            "fn main() {}\n",
            120,
        );
        let card = cards.get("node-1").unwrap();
        assert_eq!(card.state, CardState::Succeeded);
        assert_eq!(card.elapsed_ms, 120);
        assert!(card.headline().contains("succeeded"));
        assert!(card.headline().contains("read src/lib.rs"));
    }

    #[test]
    fn cancel_timeout_and_failure_are_distinguishable() {
        let mut cards = CardList::new();
        for (id, state) in [
            ("cancelled", CardState::Cancelled),
            ("timed-out", CardState::TimedOut),
            ("failed", CardState::Failed),
            ("ok", CardState::Succeeded),
        ] {
            cards.start(id, id);
            cards.finish(id, state, "", "", 10);
        }
        let headlines: Vec<String> = cards.cards().iter().map(|c| c.headline()).collect();
        assert!(headlines.iter().any(|h| h.contains("cancelled")));
        assert!(headlines.iter().any(|h| h.contains("timed out")));
        assert!(headlines.iter().any(|h| h.contains("failed")));
        assert!(headlines.iter().any(|h| h.contains("succeeded")));

        // Markers differ too, so a colorblind or monochrome terminal can
        // still tell them apart.
        let markers: Vec<&str> = cards.cards().iter().map(|c| c.state.marker()).collect();
        let mut unique = markers.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), markers.len());
    }

    #[test]
    fn a_result_without_a_start_still_becomes_visible() {
        let mut cards = CardList::new();
        cards.finish("orphan", CardState::Succeeded, "done", "output", 5);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards.get("orphan").unwrap().state, CardState::Succeeded);
    }

    #[test]
    fn expandable_output_is_bounded() {
        let mut cards = CardList::new();
        cards.start("c", "big");
        let huge = "x".repeat(MAX_CARD_DETAIL * 2);
        cards.finish("c", CardState::Succeeded, "big", huge, 1);
        let card = cards.get("c").unwrap();
        assert!(card.detail.chars().count() <= MAX_CARD_DETAIL + 1);

        assert!(!card.expanded);
        cards.toggle("c");
        assert!(cards.get("c").unwrap().expanded);
        cards.collapse_all();
        assert!(!cards.get("c").unwrap().expanded);
    }

    #[test]
    fn card_history_is_bounded() {
        let mut cards = CardList::new();
        for i in 0..(MAX_CARDS + 100) {
            cards.start(format!("n{i}"), format!("node {i}"));
            cards.finish(&format!("n{i}"), CardState::Succeeded, "", "", 1);
        }
        assert!(cards.len() <= MAX_CARDS);
        // The newest cards are the ones retained.
        assert!(cards.get(&format!("n{}", MAX_CARDS + 99)).is_some());
    }

    #[test]
    fn untrusted_output_is_sanitized_for_display() {
        // A tool result containing control sequences must not be able to
        // move the cursor or rewrite the card.
        let hostile = "\u{1b}[2J\u{1b}[1;1Hspoofed\u{1b}]0;title\u{7} real output";
        let clean = sanitize_for_display(hostile);
        assert!(!clean.contains('\u{1b}'));
        assert!(clean.contains("spoofed"));
        assert!(clean.contains("real output"));
    }

    #[test]
    fn summaries_are_single_line_and_bounded() {
        let summary = summarize_value(&json!({ "path": "src/lib.rs", "lines": 42 }));
        assert!(!summary.contains('\n'));
        assert!(summary.chars().count() <= 120);

        let long = summarize_value(&json!("first line\nsecond line"));
        assert_eq!(long, "first line");
    }

    #[test]
    fn active_state_drives_animation_not_assumption() {
        let mut cards = CardList::new();
        assert!(!cards.any_active());
        cards.start("a", "work");
        assert!(cards.any_active());
        cards.finish("a", CardState::Failed, "boom", "", 5);
        assert!(!cards.any_active());
    }

    #[test]
    fn node_status_maps_to_card_state() {
        assert_eq!(
            CardState::from_node_status(NodeStatus::Blocked),
            CardState::Waiting
        );
        assert_eq!(
            CardState::from_node_status(NodeStatus::Failed),
            CardState::Failed
        );
        assert_eq!(
            CardState::from_node_status(NodeStatus::Succeeded),
            CardState::Succeeded
        );
    }
}
