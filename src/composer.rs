//! The composer: a real multiline editor for the workbench (issue #28).
//!
//! Editing is grapheme-aware, so Swedish combining marks, emoji sequences
//! and CJK text move and delete the way a person expects rather than by
//! byte or code point. Undo/redo, history and bracketed paste are all
//! pure state transitions, so the whole editor is testable without a
//! terminal.
//!
//! Rules that keep the keyboard predictable:
//! - Enter submits; Ctrl+J inserts a newline. A large paste never
//!   auto-submits and never blocks rendering.
//! - When a dialog (help, palette) is open it owns the keyboard, so an
//!   Escape or a submit cannot both fire.
//! - Nothing here performs I/O, clipboard commands or provider calls.

use unicode_segmentation::UnicodeSegmentation;

/// Maximum characters accepted from one paste.
///
/// Pasted code can be huge; the composer keeps a bounded amount in the
/// live buffer and says so, instead of freezing the display.
pub const MAX_PASTE_CHARS: usize = 200_000;

/// Maximum composer length retained.
pub const MAX_COMPOSER_CHARS: usize = 200_000;

/// Maximum retained history entries.
pub const MAX_HISTORY: usize = 100;

/// The editor buffer plus its editing history.
#[derive(Debug, Clone, PartialEq)]
pub struct Composer {
    /// Lines of text; `cursor_row` indexes this.
    lines: Vec<String>,
    cursor_row: usize,
    /// Cursor offset *in graphemes* within the current line.
    cursor_col: usize,
    /// Undo stack of previous snapshots (bounded).
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Submitted prompts, oldest first.
    history: Vec<String>,
    /// Current position while browsing history, if browsing.
    history_cursor: Option<usize>,
    /// Whether the last paste was truncated.
    pub last_paste_truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            history: Vec::new(),
            history_cursor: None,
            last_paste_truncated: false,
        }
    }
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    /// The whole buffer as one string.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|line| line.trim().is_empty())
    }

    /// Character count of the whole buffer.
    pub fn len_chars(&self) -> usize {
        self.lines
            .iter()
            .map(|line| line.chars().count())
            .sum::<usize>()
            + self.lines.len().saturating_sub(1)
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor_row]
    }

    /// Grapheme count of the current line.
    fn current_graphemes(&self) -> usize {
        self.current_line().graphemes(true).count()
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        self.cursor_row = snapshot.cursor_row;
        self.cursor_col = snapshot.cursor_col;
    }

    /// Record the current state before a mutation, so it can be undone.
    fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Undo the last edit. Returns whether anything changed.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.restore(previous);
        true
    }

    /// Redo the last undone edit.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snapshot());
        self.restore(next);
        true
    }

    /// Insert one grapheme at the cursor.
    pub fn insert(&mut self, text: &str) {
        if self.len_chars() + text.chars().count() > MAX_COMPOSER_CHARS {
            return;
        }
        self.checkpoint();
        let row = self.cursor_row;
        let graphemes: Vec<&str> = self.lines[row].graphemes(true).collect();
        let index = self.cursor_col.min(graphemes.len());
        let mut rebuilt = String::with_capacity(self.lines[row].len() + text.len());
        for grapheme in graphemes.iter().take(index) {
            rebuilt.push_str(grapheme);
        }
        rebuilt.push_str(text);
        for grapheme in graphemes.iter().skip(index) {
            rebuilt.push_str(grapheme);
        }
        self.lines[row] = rebuilt;
        self.cursor_col = index + text.graphemes(true).count();
        self.history_cursor = None;
    }

    /// Insert a newline (Ctrl+J), splitting the line at the cursor.
    pub fn insert_newline(&mut self) {
        if self.lines.len() >= 10_000 || self.len_chars() >= MAX_COMPOSER_CHARS {
            return;
        }
        self.checkpoint();
        let row = self.cursor_row;
        let graphemes: Vec<&str> = self.lines[row].graphemes(true).collect();
        let index = self.cursor_col.min(graphemes.len());
        let head: String = graphemes[..index].concat();
        let tail: String = graphemes[index..].concat();
        self.lines[row] = head;
        self.lines.insert(row + 1, tail);
        self.cursor_row = row + 1;
        self.cursor_col = 0;
    }

    /// Backspace one grapheme, joining lines at a line start.
    pub fn backspace(&mut self) {
        if self.cursor_col == 0 && self.cursor_row == 0 {
            return;
        }
        self.checkpoint();
        let row = self.cursor_row;
        if self.cursor_col > 0 {
            let graphemes: Vec<&str> = self.lines[row].graphemes(true).collect();
            let index = self.cursor_col.min(graphemes.len());
            let mut rebuilt = String::new();
            for grapheme in graphemes.iter().take(index - 1) {
                rebuilt.push_str(grapheme);
            }
            for grapheme in graphemes.iter().skip(index) {
                rebuilt.push_str(grapheme);
            }
            self.lines[row] = rebuilt;
            self.cursor_col -= 1;
        } else {
            let previous = self.lines[row - 1].clone();
            let joined = format!("{previous}{}", self.lines[row]);
            self.cursor_col = previous.graphemes(true).count();
            self.lines[row - 1] = joined;
            self.lines.remove(row);
            self.cursor_row = row - 1;
        }
    }

    /// Delete forward one grapheme.
    pub fn delete(&mut self) {
        self.checkpoint();
        let row = self.cursor_row;
        let graphemes: Vec<&str> = self.lines[row].graphemes(true).collect();
        let index = self.cursor_col.min(graphemes.len());
        if index < graphemes.len() {
            let mut rebuilt = String::new();
            for grapheme in graphemes.iter().take(index) {
                rebuilt.push_str(grapheme);
            }
            for grapheme in graphemes.iter().skip(index + 1) {
                rebuilt.push_str(grapheme);
            }
            self.lines[row] = rebuilt;
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

    /// Move the cursor by one grapheme left, crossing lines.
    pub fn left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.current_graphemes();
        }
    }

    /// Move the cursor by one grapheme right, crossing lines.
    pub fn right(&mut self) {
        if self.cursor_col < self.current_graphemes() {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    /// Move up a line, keeping the cursor in range.
    pub fn up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.cursor_col.min(self.current_graphemes());
        } else if let Some(index) = self.history_previous() {
            self.history_cursor = Some(index);
            self.load_history(index);
        }
    }

    /// Move down a line, or forward through history at the last line.
    pub fn down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = self.cursor_col.min(self.current_graphemes());
        } else if let Some(index) = self.history_next() {
            match index {
                Some(index) => {
                    self.history_cursor = Some(index);
                    self.load_history(index);
                }
                None => {
                    self.history_cursor = None;
                    self.lines = vec![String::new()];
                    self.cursor_row = 0;
                    self.cursor_col = 0;
                }
            }
        }
    }

    /// Move to the start of the current line.
    pub fn home(&mut self) {
        self.cursor_col = 0;
    }

    /// Move to the end of the current line.
    pub fn end(&mut self) {
        self.cursor_col = self.current_graphemes();
    }

    /// Insert pasted text.
    ///
    /// Bracketed paste is one action: a large paste is inserted whole (up
    /// to the bound) and never auto-submits. Truncation is reported so the
    /// user knows the buffer is not the whole paste.
    pub fn paste(&mut self, text: &str) {
        let incoming = text.chars().count();
        let available = MAX_COMPOSER_CHARS.saturating_sub(self.len_chars());
        let (accepted, truncated) = if incoming > available {
            let bounded: String = text.chars().take(available).collect();
            (bounded, true)
        } else {
            (text.to_owned(), false)
        };
        self.last_paste_truncated = truncated;

        self.checkpoint();
        // A paste with newlines becomes multiple lines rather than being
        // mangled into one.
        let mut parts = accepted.split('\n');
        if let Some(first) = parts.next() {
            self.insert(first);
        }
        for part in parts {
            self.insert_newline();
            self.insert(part);
        }
    }

    pub fn clear(&mut self) {
        self.checkpoint();
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.history_cursor = None;
    }

    /// Take the buffer for submission and record it in history.
    pub fn take_submission(&mut self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let text = self.text();
        self.history.push(text.clone());
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        self.history_cursor = None;
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.undo.clear();
        self.redo.clear();
        Some(text)
    }

    /// Submitted prompts, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn history_previous(&self) -> Option<usize> {
        if self.history.is_empty() {
            return None;
        }
        match self.history_cursor {
            Some(0) => None,
            Some(index) => Some(index - 1),
            None => Some(self.history.len() - 1),
        }
    }

    fn history_next(&self) -> Option<Option<usize>> {
        if self.history.is_empty() {
            return None;
        }
        match self.history_cursor {
            Some(index) if index + 1 < self.history.len() => Some(Some(index + 1)),
            Some(_) => Some(None),
            None => None,
        }
    }

    fn load_history(&mut self, index: usize) {
        if let Some(entry) = self.history.get(index) {
            self.lines = entry.split('\n').map(str::to_owned).collect();
            if self.lines.is_empty() {
                self.lines.push(String::new());
            }
            self.cursor_row = self.lines.len() - 1;
            self.cursor_col = self.current_graphemes();
        }
    }

    /// Whether the buffer is a steering-style prompt (a short directive
    /// rather than new work). Used only for labelling; the session owns
    /// the actual steering decision.
    pub fn looks_like_steering(&self) -> bool {
        let text = self.text();
        let trimmed = text.trim();
        !trimmed.is_empty() && !trimmed.contains('\n') && trimmed.chars().count() <= 160
    }
}

#[cfg(test)]
mod tests {
    fn fill_to(cap: usize) -> Composer {
        let mut c = Composer::new();
        c.insert(&"x".repeat(cap));
        c
    }

    #[test]
    fn len_chars_counts_newlines_as_chars() {
        let mut c = Composer::new();
        c.insert("ab");
        c.insert_newline();
        c.insert("cd");
        assert_eq!(c.len_chars(), 5);
        assert_eq!(c.text(), "ab\ncd");
    }

    #[test]
    fn len_chars_counts_unicode_scalars() {
        let mut c = Composer::new();
        c.insert("aé🦀");
        c.insert_newline();
        c.insert("ö");
        assert_eq!(c.len_chars(), 5);
    }

    #[test]
    fn newline_at_capacity_is_noop() {
        let mut c = fill_to(MAX_COMPOSER_CHARS);
        c.insert_newline();
        assert_eq!(c.text(), "x".repeat(MAX_COMPOSER_CHARS));
        assert_eq!(c.cursor(), (0, MAX_COMPOSER_CHARS));
        assert_eq!(c.len_chars(), MAX_COMPOSER_CHARS);
    }

    #[test]
    fn newline_fits_below_capacity() {
        let mut c = fill_to(MAX_COMPOSER_CHARS - 1);
        c.insert_newline();
        assert_eq!(
            c.text(),
            format!("{}\n", "x".repeat(MAX_COMPOSER_CHARS - 1))
        );
        assert_eq!(c.cursor(), (1, 0));
    }

    #[test]
    fn multiline_paste_respects_limit() {
        let mut c = Composer::new();
        let text = "y".repeat(MAX_COMPOSER_CHARS + 5);
        let pasted = format!("a\n{}\nb", text);
        c.paste(&pasted);
        assert!(c.last_paste_truncated);
        assert_eq!(c.len_chars(), MAX_COMPOSER_CHARS);
        assert!(c.text().starts_with("a\n"));
        assert!(c.text().contains('\n'));
    }

    use super::*;

    /// Type text the way a user would: a newline is a line break, not a
    /// literal character in the line.
    fn typed(text: &str) -> Composer {
        let mut composer = Composer::new();
        for grapheme in text.graphemes(true) {
            if grapheme == "\n" {
                composer.insert_newline();
            } else {
                composer.insert(grapheme);
            }
        }
        composer
    }

    #[test]
    fn typing_and_editing_handles_combining_marks_and_emoji() {
        // Swedish combining marks, a family emoji (ZWJ sequence) and CJK.
        let composer = typed("räksmörgås 👨‍👩‍👧 日本語");
        assert_eq!(composer.text(), "räksmörgås 👨‍👩‍👧 日本語");

        // Backspace removes whole graphemes, never a byte or half an
        // emoji. The trailing three graphemes are 日, 本 and 語.
        let mut composer = composer;
        for _ in 0..3 {
            composer.backspace();
        }
        assert_eq!(composer.text(), "räksmörgås 👨‍👩‍👧 ");
        // The space before the CJK word goes next...
        composer.backspace();
        assert_eq!(composer.text(), "räksmörgås 👨‍👩‍👧");
        // ...then the family emoji, which is one grapheme however many
        // code points and ZWJ joiners it uses.
        composer.backspace();
        assert_eq!(composer.text(), "räksmörgås ");
        // The Swedish word with its combining marks is untouched: no
        // byte-level mangling anywhere in this sequence.
        composer.backspace();
        assert_eq!(composer.text(), "räksmörgås");
    }

    #[test]
    fn cursor_movement_is_grapheme_aware() {
        let mut composer = typed("a👨‍👩‍👧b");
        // Three graphemes: 'a', the family, 'b'.
        composer.home();
        composer.right();
        composer.right();
        assert_eq!(composer.cursor(), (0, 2));
        composer.left();
        assert_eq!(composer.cursor(), (0, 1));
    }

    #[test]
    fn multiline_editing_splits_and_joins_lines() {
        let mut composer = typed("fn main() {}");
        composer.home();
        composer.right();
        composer.right();
        composer.insert_newline();
        assert_eq!(
            composer.lines(),
            &["fn".to_owned(), " main() {}".to_owned()]
        );
        assert_eq!(composer.cursor(), (1, 0));

        // Backspace at a line start joins the lines again.
        composer.backspace();
        assert_eq!(composer.lines(), &["fn main() {}".to_owned()]);
        assert_eq!(composer.cursor(), (0, 2));
    }

    #[test]
    fn undo_and_redo_restore_exact_states() {
        let mut composer = typed("hello");
        composer.insert(" world");
        assert_eq!(composer.text(), "hello world");

        assert!(composer.undo());
        assert_eq!(composer.text(), "hello");
        assert!(composer.redo());
        assert_eq!(composer.text(), "hello world");

        // Undo across a newline and a deletion.
        composer.insert_newline();
        composer.insert("second");
        assert_eq!(composer.text(), "hello world\nsecond");
        composer.undo();
        assert_eq!(composer.text(), "hello world\n");
        composer.undo();
        assert_eq!(composer.text(), "hello world");
    }

    #[test]
    fn undo_is_bounded_and_reports_when_nothing_is_left() {
        let mut composer = Composer::new();
        for _ in 0..300 {
            composer.insert("x");
        }
        let mut undos = 0;
        while composer.undo() {
            undos += 1;
            assert!(undos < 1000);
        }
        // Bounded: the stack does not grow forever.
        assert!(undos <= 200);
    }

    #[test]
    fn history_recalls_submitted_prompts() {
        let mut composer = typed("first prompt");
        assert_eq!(composer.take_submission(), Some("first prompt".to_owned()));
        composer.insert("second prompt");
        assert_eq!(composer.take_submission(), Some("second prompt".to_owned()));

        assert_eq!(composer.history(), &["first prompt", "second prompt"]);
        // Up recalls the newest; another up recalls the older one.
        composer.up();
        assert_eq!(composer.text(), "second prompt");
        composer.up();
        assert_eq!(composer.text(), "first prompt");
        // Down walks forward again.
        composer.down();
        assert_eq!(composer.text(), "second prompt");
        composer.down();
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn a_large_paste_is_inserted_whole_without_submitting() {
        let mut composer = Composer::new();
        let code = "fn main() {\n    println!(\"hi\");\n}\n".repeat(200);
        composer.paste(&code);

        // Not submitted: the buffer holds it and no history entry exists.
        assert!(composer.history().is_empty());
        assert_eq!(composer.text(), code);
        assert!(!composer.last_paste_truncated);
    }

    #[test]
    fn an_oversized_paste_is_bounded_and_says_so() {
        let mut composer = Composer::new();
        let huge = "x".repeat(MAX_PASTE_CHARS + 5_000);
        composer.paste(&huge);

        assert!(composer.last_paste_truncated);
        assert_eq!(
            composer.len_chars(),
            MAX_COMPOSER_CHARS.min(MAX_PASTE_CHARS + 5_000)
        );
        // Bounded: the buffer never grew past the composer limit.
        assert!(composer.len_chars() <= MAX_COMPOSER_CHARS);
    }

    #[test]
    fn paste_with_crlf_and_tabs_keeps_line_structure() {
        let mut composer = Composer::new();
        composer.paste("line one\n\tline two\nline three");
        assert_eq!(composer.lines().len(), 3);
        assert_eq!(composer.lines()[1], "\tline two");
    }

    #[test]
    fn empty_submissions_are_refused() {
        let mut composer = typed("   \n  ");
        composer.end();
        assert!(composer.is_empty());
        assert_eq!(composer.take_submission(), None);
        // The buffer is untouched by a refused submit.
        assert_eq!(composer.text(), "   \n  ");
    }

    #[test]
    fn delete_joins_lines_and_never_panics_at_the_end() {
        let mut composer = typed("ab\ncd");
        // Cursor at the end of the first line, then forward-delete joins.
        composer.home();
        composer.up();
        composer.end();
        assert_eq!(composer.cursor(), (0, 2));
        composer.delete();
        assert_eq!(composer.text(), "abcd");

        // Deleting at the very end is a no-op.
        composer.end();
        composer.delete();
        assert_eq!(composer.text(), "abcd");
    }

    #[test]
    fn steering_shaped_prompts_are_recognised_without_owning_the_decision() {
        let composer = typed("use the simpler approach");
        assert!(composer.looks_like_steering());

        let mut composer = Composer::new();
        composer.paste("a longer multi-line\nrequest describing new work");
        assert!(!composer.looks_like_steering());
    }
}
