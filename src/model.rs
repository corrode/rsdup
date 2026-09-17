use std::{fmt, path::PathBuf};

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Profile {
    #[default]
    Balanced,
    Strict,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DetectionMode {
    Exact,
    #[default]
    NearMiss,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortOrder {
    #[default]
    Occurrences,
    Impact,
    Size,
    Spread,
    Path,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub min_tokens: usize,
    pub min_lines: usize,
    pub profile: Profile,
    pub mode: DetectionMode,
    pub max_holes: usize,
    pub ignore_tests: bool,
    pub sort: SortOrder,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            min_tokens: 40,
            min_lines: 5,
            profile: Profile::Balanced,
            mode: DetectionMode::NearMiss,
            max_holes: 1,
            ignore_tests: true,
            sort: SortOrder::Occurrences,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Occurrence {
    pub path: PathBuf,
    pub start: Position,
    pub end: Position,
    /// Original source captured during scanning; excluded from the syntax JSON schema.
    #[serde(skip)]
    pub source: String,
    #[serde(skip)]
    pub(crate) file_id: usize,
    #[serde(skip)]
    pub(crate) byte_start: usize,
    #[serde(skip)]
    pub(crate) byte_end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CloneClass {
    pub id: String,
    pub tokens: usize,
    pub lines: usize,
    pub holes: usize,
    pub occurrences: Vec<Occurrence>,
    #[serde(skip)]
    pub syntax_template: String,
}

impl fmt::Display for CloneClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.holes == 0 {
            "duplicate"
        } else {
            "near duplicate"
        };
        write!(
            formatter,
            "{kind} {}: {} anchored tokens, {} lines, {} occurrences",
            self.id,
            self.tokens,
            self.lines,
            self.occurrences.len()
        )?;
        match self.holes {
            0 => Ok(()),
            1 => write!(formatter, ", 1 syntax hole"),
            holes => write!(formatter, ", {holes} syntax holes"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Summary {
    pub files: usize,
    pub clone_classes: usize,
    pub duplicated_lines: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub summary: Summary,
    pub clones: Vec<CloneClass>,
}

#[cfg(test)]
mod tests {
    use super::CloneClass;

    fn clone_class(holes: usize) -> CloneClass {
        CloneClass {
            id: "abc123".to_owned(),
            tokens: 42,
            lines: 7,
            holes,
            occurrences: Vec::new(),
            syntax_template: String::new(),
        }
    }

    #[test]
    fn exact_clone_display_has_no_hole_suffix() {
        assert_eq!(
            clone_class(0).to_string(),
            "duplicate abc123: 42 anchored tokens, 7 lines, 0 occurrences"
        );
    }

    #[test]
    fn near_clone_display_pluralizes_holes() {
        assert_eq!(
            clone_class(1).to_string(),
            "near duplicate abc123: 42 anchored tokens, 7 lines, 0 occurrences, 1 syntax hole"
        );
        assert_eq!(
            clone_class(2).to_string(),
            "near duplicate abc123: 42 anchored tokens, 7 lines, 0 occurrences, 2 syntax holes"
        );
    }
}
