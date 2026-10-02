use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

use super::{Composer, MAX_COMPOSER_CHARS, MAX_HISTORY, MAX_LINES, Snapshot};

const MAX_HISTORY_CHARS: usize = 1_000_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerMemory {
    draft: String,
    cursor: (usize, usize),
    history: Vec<String>,
}

impl Composer {
    pub fn memory(&self) -> ComposerMemory {
        // Browsing old prompts must not replace the unfinished draft on disk.
        let text = self.text();
        let draft = self.history_draft.as_ref().filter(|_| {
            self.history_cursor
                .and_then(|index| self.history.get(index))
                == Some(&text)
        });
        let mut budget = MAX_HISTORY_CHARS;
        let mut history = Vec::new();
        for prompt in self.history.iter().rev().take(MAX_HISTORY) {
            let size = prompt.chars().count();
            if size > budget {
                break;
            }
            budget -= size;
            history.push(prompt.clone());
        }
        history.reverse();
        ComposerMemory {
            draft: draft.map_or(text, |draft| draft.lines.join("\n")),
            cursor: draft.map_or_else(
                || self.cursor(),
                |draft| (draft.cursor_row, draft.cursor_col),
            ),
            history,
        }
    }

    pub fn from_memory(memory: ComposerMemory) -> Result<Self, &'static str> {
        if memory.draft.chars().count() > MAX_COMPOSER_CHARS
            || memory.draft.split('\n').count() > MAX_LINES
            || memory.history.len() > MAX_HISTORY
            || memory.history.iter().any(|entry| {
                entry.chars().count() > MAX_COMPOSER_CHARS || entry.split('\n').count() > MAX_LINES
            })
            || memory
                .history
                .iter()
                .map(|entry| entry.chars().count())
                .sum::<usize>()
                > MAX_HISTORY_CHARS
        {
            return Err("saved input exceeds the editor's limits");
        }
        let lines: Vec<String> = memory.draft.split('\n').map(str::to_owned).collect();
        let (cursor_row, cursor_col) = memory.cursor;
        if lines
            .get(cursor_row)
            .is_none_or(|line| cursor_col > line.graphemes(true).count())
        {
            return Err("saved cursor is outside the draft");
        }
        let mut composer = Self::new();
        composer.restore(Snapshot {
            lines,
            cursor_row,
            cursor_col,
        });
        composer.history = memory.history;
        Ok(composer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_restores_multiline_graphemes_cursor_and_history() {
        let mut composer = Composer::new();
        composer.paste("earlier task");
        composer.take_submission();
        composer.paste("a\u{030a}👩‍💻\nunfinished 界");
        composer.left();
        let mut restored = Composer::from_memory(composer.memory()).unwrap();
        assert_eq!(restored.text(), composer.text());
        assert_eq!(restored.cursor(), composer.cursor());
        assert_eq!(restored.history(), &["earlier task"]);
        restored.insert("next ");
        assert_eq!(restored.text(), "a\u{030a}👩‍💻\nunfinished next 界");
    }

    #[test]
    fn browsing_history_does_not_overwrite_the_draft() {
        let mut composer = Composer::new();
        composer.insert("previous prompt");
        composer.take_submission();
        composer.insert("unfinished");
        composer.left();
        let before = composer.memory();
        composer.up();
        assert_eq!(composer.text(), "previous prompt");
        assert_eq!(composer.memory(), before);
        composer.backspace();
        assert_eq!(composer.memory().draft, "previous promp");
        composer.insert(" edited");
        assert!(composer.memory().draft.contains("edited"));
    }

    #[test]
    fn history_has_a_total_budget_and_keeps_the_newest_prompts() {
        let mut composer = Composer::new();
        for index in 0..7 {
            composer.paste(&format!("{index}{}", "x".repeat(MAX_COMPOSER_CHARS - 1)));
            composer.take_submission();
        }
        let memory = composer.memory();
        assert_eq!(memory.history.len(), 5);
        assert!(memory.history[0].starts_with('2'));
        assert!(memory.history[4].starts_with('6'));
        assert!(Composer::from_memory(memory).is_ok());
    }

    #[test]
    fn saved_input_cannot_bypass_the_line_limit() {
        for (draft, history) in [
            ("\n".repeat(MAX_LINES), vec![]),
            (String::new(), vec!["\n".repeat(MAX_LINES)]),
        ] {
            assert!(
                Composer::from_memory(ComposerMemory {
                    draft,
                    cursor: (0, 0),
                    history,
                })
                .is_err()
            );
        }
    }

    #[test]
    fn invalid_saved_cursor_is_reported() {
        for cursor in [(1, 0), (0, 2)] {
            let memory = ComposerMemory {
                draft: "a\u{030a}".to_owned(),
                cursor,
                history: vec![],
            };
            assert!(Composer::from_memory(memory).is_err());
        }
    }
}
