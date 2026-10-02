use serde::Serialize;
use serde_json::{Value, json};

use crate::Candidate;

const LOG_BUDGET: usize = 6000;
const FRAME_CANDIDATES: usize = 6;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Failure {
    pub id: String,
    pub source: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub evidence: String,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct RepairEvidence {
    pub failures: Vec<Failure>,
    pub focus: Option<String>,
}

impl RepairEvidence {
    pub fn new(failures: Vec<(String, String, Option<i32>, String)>) -> Self {
        let budget = LOG_BUDGET / failures.len().max(1);
        Self {
            failures: failures
                .into_iter()
                .enumerate()
                .map(|(index, (source, status, exit_code, output))| Failure {
                    id: format!("failure-{index}"),
                    source,
                    status,
                    exit_code,
                    evidence: diagnostic_excerpt(&output, budget),
                })
                .collect(),
            focus: None,
        }
    }

    pub fn candidates(&self) -> Vec<Candidate> {
        self.failures
            .iter()
            .take(FRAME_CANDIDATES)
            .map(|failure| {
                Candidate::new(
                    &failure.id,
                    format!("Inspect failure in {} ({})", failure.source, failure.status),
                )
            })
            .collect()
    }

    pub fn observation(&self) -> Value {
        json!({
            "untrusted_failures": self.failures.iter().take(FRAME_CANDIDATES).map(|failure| json!({
                "id": failure.id,
                "source": failure.source.chars().take(100).collect::<String>(),
                "status": failure.status,
                "exit_code": failure.exit_code,
                "evidence": diagnostic_excerpt(&failure.evidence, 350),
            })).collect::<Vec<_>>(),
            "omitted_failures": self.failures.len().saturating_sub(FRAME_CANDIDATES),
        })
    }

    pub fn focus(&mut self, id: &str) -> bool {
        if self
            .failures
            .iter()
            .take(FRAME_CANDIDATES)
            .any(|failure| failure.id == id)
        {
            self.focus = Some(id.to_owned());
            true
        } else {
            false
        }
    }
}

pub(crate) fn diagnostic_excerpt(output: &str, budget: usize) -> String {
    if output.len() <= budget {
        return output.to_owned();
    }
    let lines: Vec<_> = output.lines().collect();
    let mut selected = std::collections::BTreeSet::new();
    let mut remaining = budget.saturating_sub(80);
    let mut add = |index: usize| {
        if selected.contains(&index) {
            return;
        }
        let cost = lines[index].len() + 24;
        if cost <= remaining {
            remaining -= cost;
            selected.insert(index);
        }
    };
    let mut seen = std::collections::BTreeSet::new();
    for (index, line) in lines.iter().enumerate() {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with("error")
            || lower.starts_with("fail")
            || lower.contains("panicked at")
            || lower.contains("assertion")
            || lower.starts_with("caused by:")
            || lower.starts_with("traceback")
            || lower.ends_with("... failed")
        {
            if !seen.insert(line.trim()) {
                continue;
            }
            for nearby in index.saturating_sub(1)..=(index + 3).min(lines.len() - 1) {
                add(nearby);
            }
        }
    }
    for index in (0..lines.len()).rev().take(8) {
        add(index);
    }
    for index in 0..lines.len().min(3) {
        add(index);
    }
    if selected.is_empty() {
        return bounded_tail(output, budget);
    }
    let mut result = String::new();
    let mut previous = None;
    for index in selected {
        if previous != index.checked_sub(1) || (previous.is_none() && index > 0) {
            result.push_str("[... omitted ...]\n");
        }
        result.push_str(&format!("L{}: {}\n", index + 1, lines[index]));
        previous = Some(index);
    }
    if previous != lines.len().checked_sub(1) {
        result.push_str("[... omitted ...]\n");
    }
    bounded_tail(&result, budget)
}

pub(crate) fn bounded_tail(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let marker = "\n[... omitted ...]\n";
    if budget < marker.len() {
        return String::new();
    }
    let remaining = budget - marker.len();
    let mut head = remaining / 3;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (remaining - head);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{marker}{}", &text[..head], &text[tail..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_after_noisy_build_output_keep_locations_and_causes() {
        let log = format!(
            "{}error[E0308]: mismatched types\n --> src/lib.rs:42:9\n expected String, found bool\n{}test result: FAILED\n",
            "compiling crate\n".repeat(600),
            "irrelevant\n".repeat(200)
        );
        let excerpt = diagnostic_excerpt(&log, 800);
        assert!(excerpt.contains("error[E0308]"));
        assert!(excerpt.contains("src/lib.rs:42:9"));
        assert!(excerpt.contains("expected String, found bool"));
        assert!(excerpt.contains("test result: FAILED"));
        assert!(excerpt.len() <= 800, "{}", excerpt.len());
    }

    #[test]
    fn unicode_and_long_single_lines_stay_bounded_and_keep_the_tail() {
        let log = format!("{}final failure", "界".repeat(4000));
        let excerpt = diagnostic_excerpt(&log, 400);
        assert!(excerpt.len() <= 400);
        assert!(excerpt.ends_with("final failure"));
    }

    #[test]
    fn focus_can_only_name_observed_evidence_and_never_discards_other_failures() {
        let mut packet = RepairEvidence::new(vec![
            (
                "check/build".into(),
                "failed".into(),
                Some(1),
                "compiler error".into(),
            ),
            (
                "check/test".into(),
                "failed".into(),
                Some(101),
                "test regression".into(),
            ),
        ]);
        assert!(!packet.focus("invented"));
        assert!(packet.focus("failure-1"));
        assert_eq!(packet.failures.len(), 2);
        assert!(packet.observation().to_string().contains("test regression"));
    }
}
