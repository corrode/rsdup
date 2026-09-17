//! Bounded, exact-text line comparison, independent of terminal rendering.
use std::collections::HashMap;

/// Maximum LCS table cells (about 4 MiB); runtime beyond input scanning is bounded.
const MAX_CELLS: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineChange {
    Equal,
    Removed,
    Added,
    Changed,
}

#[derive(Debug)]
pub(crate) struct SnippetDiff {
    pub left: Vec<LineChange>,
    pub right: Vec<LineChange>,
    /// The unmatched middle exceeded the budget: its marks are conservative,
    /// not a minimal edit script or a claim that every marked line differs.
    pub limited: bool,
}

impl SnippetDiff {
    pub fn between(left: &str, right: &str) -> Self {
        Self::with_limit(left, right, MAX_CELLS)
    }

    fn with_limit(left: &str, right: &str, limit: usize) -> Self {
        // Intern exact lines once so long lines cannot multiply LCS comparison
        // cost. Keep line terminators: CRLF, indentation and final-newline
        // differences count, even when the terminal displays them identically.
        let mut ids = HashMap::new();

        let mut encode = |text| {
            let mut result = Vec::new();
            for line in split_lines(text) {
                let next = ids.len();
                result.push(*ids.entry(line).or_insert(next));
            }
            result
        };
        let a = encode(left);
        let b = encode(right);
        let mut diff = Self {
            left: vec![LineChange::Equal; a.len()],
            right: vec![LineChange::Equal; b.len()],
            limited: false,
        };
        let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
        let suffix = a[prefix..]
            .iter()
            .rev()
            .zip(b[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let end_a = a.len() - suffix;
        let end_b = b.len() - suffix;
        let n = end_a - prefix;
        let m = end_b - prefix;
        if n == 0 || m == 0 {
            diff.hunk(prefix, end_a, prefix, end_b);
            return diff;
        }
        let cells = n
            .checked_add(1)
            .and_then(|rows| m.checked_add(1)?.checked_mul(rows));
        let Some(cells) = cells.filter(|&cells| cells <= limit) else {
            diff.left[prefix..end_a].fill(LineChange::Changed);
            diff.right[prefix..end_b].fill(LineChange::Changed);
            diff.limited = true;
            return diff;
        };
        let width = m + 1;
        let mut lcs = vec![0u32; cells];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * width + j] = if a[prefix + i] == b[prefix + j] {
                    1 + lcs[(i + 1) * width + j + 1]
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        let (mut start_a, mut start_b) = (prefix, prefix);
        while i < n && j < m {
            if a[prefix + i] == b[prefix + j] {
                diff.hunk(start_a, prefix + i, start_b, prefix + j);
                i += 1;
                j += 1;
                start_a = prefix + i;
                start_b = prefix + j;
            } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
        diff.hunk(start_a, end_a, start_b, end_b);
        diff
    }

    fn hunk(&mut self, a: usize, end_a: usize, b: usize, end_b: usize) {
        let paired = (end_a - a).min(end_b - b);
        self.left[a..a + paired].fill(LineChange::Changed);
        self.right[b..b + paired].fill(LineChange::Changed);
        self.left[a + paired..end_a].fill(LineChange::Removed);
        self.right[b + paired..end_b].fill(LineChange::Added);
    }
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        vec![""]
    } else {
        text.split_inclusive('\n').collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{LineChange::*, *};

    #[test]
    fn inserts_align_without_cascading_and_deletes_reverse() {
        let a = "start\nkeep\nend\n";
        let b = "start\ninsert\nkeep\nend\n";
        let diff = SnippetDiff::between(a, b);
        assert_eq!(diff.left, [Equal, Equal, Equal]);
        assert_eq!(diff.right, [Equal, Added, Equal, Equal]);
        assert_eq!(
            SnippetDiff::between(b, a).left,
            [Equal, Removed, Equal, Equal]
        );
    }

    #[test]
    fn unicode_whitespace_and_terminators_are_exact() {
        for (a, b) in [
            ("日本e\u{301}\n", "日本é\n"),
            (" a\n", "a\n"),
            ("a\n", "a"),
            ("a\r\n", "a\n"),
        ] {
            let diff = SnippetDiff::between(a, b);
            assert_eq!(diff.left, [Changed]);
            assert_eq!(diff.right, [Changed]);
        }
        for text in ["", "\n", "a\n\n", "日本語", "a\r\n"] {
            let diff = SnippetDiff::between(text, text);
            assert!(diff.left.iter().chain(&diff.right).all(|c| *c == Equal));
            assert!(!diff.limited);
        }
    }

    #[test]
    fn interior_alignment_and_blank_lines() {
        let diff = SnippetDiff::between("old\nkeep\nlast\n", "new\nkeep\n\nother\n");
        assert_eq!(diff.left, [Changed, Equal, Changed]);
        assert_eq!(diff.right, [Changed, Equal, Changed, Added]);
    }

    #[test]
    fn budget_marks_only_middle_and_never_claims_equality() {
        assert!(!SnippetDiff::with_limit("a", "b", 4).limited);
        assert!(SnippetDiff::with_limit("a", "b", 3).limited);
        let diff = SnippetDiff::with_limit("same\na\nkeep\nb\nend", "same\nc\nkeep\nd\nend", 4);
        assert!(diff.limited);
        assert_eq!(diff.left, [Equal, Changed, Changed, Changed, Equal]);
        assert_eq!(diff.right, diff.left);
        let diff = SnippetDiff::with_limit("a\nb\n", "a\ninsert\nb\n", 0);
        assert!(!diff.limited);
        assert_eq!(diff.right, [Equal, Added, Equal]);
        let diff = SnippetDiff::between(&"a\n".repeat(6000), &"b\n".repeat(6000));
        assert!(diff.limited);
        assert!(diff.left.iter().all(|c| *c == Changed));
        let long = "日".repeat(20_000);
        assert_eq!(
            SnippetDiff::between(&long, &format!("{long}x")).left,
            [Changed]
        );
    }
}
