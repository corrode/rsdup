use std::path::PathBuf;

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
            max_holes: 2,
            ignore_tests: false,
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
