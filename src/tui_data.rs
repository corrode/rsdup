//! Presentation snapshots shared by the syntax and semantic browsers.
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use ra_ap_syntax::{
    AstNode, Edition, SourceFile,
    ast::{self, HasName},
};
use rsdup::{Position, Report, semantic::SemanticReport};

pub(crate) struct BrowserReport {
    pub groups: Vec<MatchGroup>,
    pub files: usize,
}

pub(crate) struct MatchGroup {
    pub title: String,
    pub occurrences: Vec<Snippet>,
    pub edges: Vec<PairEvidence>,
}

pub(crate) struct Snippet {
    pub name: String,
    pub path: PathBuf,
    pub start_line: usize,
    pub start_column: usize,
    /// The original matched text, used for pairwise diffing.
    pub source: String,
    /// Shared surrounding file text, or just the snapshot if context is unavailable.
    pub display_source: Arc<str>,
    pub display_start_line: usize,
    pub match_start_line: usize,
    pub match_end_line: usize,
    pub test_context: bool,
    pub warning: Option<String>,
}

pub(crate) struct PairEvidence {
    pub left: usize,
    pub right: usize,
    pub cosine: f64,
    pub exact_body: bool,
}

struct Source {
    text: Arc<str>,
    functions: Vec<(usize, usize, String)>,
}

type Sources = HashMap<PathBuf, Result<Source, String>>;

const MAX_CONTEXT_BYTES: u64 = 2 * 1024 * 1024;

impl Snippet {
    fn new(name: String, path: &Path, start: &Position, source: &str, test_context: bool) -> Self {
        Self {
            name,
            path: path.to_path_buf(),
            start_line: start.line,
            start_column: start.column,
            source: source.to_owned(),
            display_source: Arc::from(source),
            display_start_line: start.line,
            match_start_line: start.line,
            match_end_line: start
                .line
                .saturating_add(source.lines().count().saturating_sub(1)),
            test_context,
            warning: None,
        }
    }

    fn with_context(&mut self, current: &Result<Source, String>, end: &Position) {
        let start = Position {
            line: self.start_line,
            column: self.start_column,
        };
        match current {
            Ok(file) if slice(&file.text, &start, end) == Some(self.source.as_str()) => {
                self.display_source = Arc::clone(&file.text);
                self.display_start_line = 1;
            }
            Ok(_) => self.warning = Some("Source changed since the scan; showing only the scanned match. Editor location may be stale.".into()),
            Err(error) => self.warning = Some(format!("Context unavailable; showing only the scanned match. {error}")),
        }
    }
}

fn load<'a>(sources: &'a mut Sources, path: &Path) -> &'a Result<Source, String> {
    sources.entry(path.to_path_buf()).or_insert_with(|| {
        let metadata =
            fs::metadata(path).map_err(|error| format!("Cannot inspect source: {error}"))?;
        if !metadata.is_file() {
            return Err("Not a regular source file".into());
        }
        if metadata.len() > MAX_CONTEXT_BYTES {
            return Err("File exceeds the 2 MiB context limit".into());
        }
        let file = fs::File::open(path).map_err(|error| format!("Cannot open source: {error}"))?;
        let mut bytes = Vec::new();
        file.take(MAX_CONTEXT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Cannot read source: {error}"))?;
        if bytes.len() as u64 > MAX_CONTEXT_BYTES {
            return Err("File exceeds the 2 MiB context limit".into());
        }
        let text =
            String::from_utf8(bytes).map_err(|error| format!("Source is not UTF-8: {error}"))?;
        let tree = SourceFile::parse(&text, Edition::Edition2024).tree();
        let functions = tree
            .syntax()
            .descendants()
            .filter_map(ast::Fn::cast)
            .filter_map(|function| {
                let name = function.name()?.to_string();
                let range = function.syntax().text_range();
                Some((usize::from(range.start()), usize::from(range.end()), name))
            })
            .collect();
        Ok(Source {
            text: text.into(),
            functions,
        })
    })
}

fn offset(source: &str, position: &Position) -> Option<usize> {
    let line_start = if position.line == 1 {
        0
    } else {
        source
            .match_indices('\n')
            .nth(position.line.checked_sub(2)?)
            .map(|(offset, _)| offset + 1)?
    };
    let line = source.get(line_start..)?.split('\n').next()?;
    let column = position.column.checked_sub(1)?;
    let byte = if column == line.chars().count() {
        line.len()
    } else {
        line.char_indices().nth(column)?.0
    };
    Some(line_start + byte)
}

fn slice<'a>(source: &'a str, start: &Position, end: &Position) -> Option<&'a str> {
    source.get(offset(source, start)?..offset(source, end)?)
}

fn group_title(occurrences: &[Snippet]) -> String {
    let Some(first) = occurrences.first() else {
        return "Empty group".into();
    };
    match occurrences
        .iter()
        .find(|snippet| snippet.name != first.name)
    {
        Some(other) => format!("{} / {}", first.name, other.name),
        None => first.name.clone(),
    }
}

pub(crate) fn semantic(report: &SemanticReport) -> BrowserReport {
    let mut sources = Sources::new();
    let groups = report
        .clusters
        .iter()
        .map(|cluster| {
            let occurrences: Vec<_> = cluster
                .members
                .iter()
                .map(|&index| {
                    let unit = &report.units[index];
                    let tree = SourceFile::parse(&unit.context, Edition::Edition2024).tree();
                    let name = tree
                        .syntax()
                        .descendants()
                        .find_map(ast::Fn::cast)
                        .and_then(|function| function.name())
                        .map(|name| format!("{name}()"))
                        .unwrap_or_else(|| unit.kind.clone());
                    let name = match unit.kind.as_str() {
                        "closure" => format!("{name} · closure"),
                        "async" => format!("{name} · async"),
                        _ => name,
                    };
                    let mut snippet = Snippet::new(
                        name,
                        &unit.path,
                        &unit.start,
                        &unit.source,
                        unit.test_context,
                    );
                    snippet.with_context(load(&mut sources, &unit.path), &unit.end);
                    snippet
                })
                .collect();
            let local: HashMap<_, _> = cluster
                .members
                .iter()
                .enumerate()
                .map(|(local, &global)| (global, local))
                .collect();
            let edges: Vec<_> = cluster
                .edges
                .iter()
                .map(|edge| PairEvidence {
                    left: local[&edge.left],
                    right: local[&edge.right],
                    cosine: edge.cosine,
                    exact_body: edge.exact_body,
                })
                .collect();

            MatchGroup {
                title: group_title(&occurrences),
                occurrences,
                edges,
            }
        })
        .collect();
    BrowserReport {
        groups,
        files: report.summary.files,
    }
}

pub(crate) fn syntax(report: &Report) -> BrowserReport {
    let mut sources = Sources::new();
    let groups = report
        .clones
        .iter()
        .map(|clone| {
            let occurrences: Vec<_> = clone
                .occurrences
                .iter()
                .map(|occurrence| {
                    let mut snippet = Snippet::new(
                        "statement sequence".into(),
                        &occurrence.path,
                        &occurrence.start,
                        &occurrence.source,
                        false,
                    );
                    let current = load(&mut sources, &occurrence.path);
                    snippet.with_context(current, &occurrence.end);
                    if snippet.warning.is_none()
                        && let Ok(source) = current
                        && let Some(start) = offset(&source.text, &occurrence.start)
                        && let Some((_, _, name)) = source
                            .functions
                            .iter()
                            .filter(|(from, to, _)| *from <= start && start < *to)
                            .min_by_key(|(from, to, _)| to - from)
                    {
                        snippet.name = format!("{name}()");
                    }
                    snippet
                })
                .collect();
            MatchGroup {
                title: group_title(&occurrences),
                occurrences,
                edges: Vec::new(),
            }
        })
        .collect();
    BrowserReport {
        groups,
        files: report.summary.files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsdup::{
        Options,
        semantic::{self, Embedder, SemanticOptions},
    };

    struct Constant;
    impl Embedder for Constant {
        fn embed(&mut self, _: &str) -> anyhow::Result<Vec<f64>> {
            Ok(vec![1.0])
        }
    }

    #[test]
    fn surrounding_context_preserves_match_boundaries_and_falls_back_safely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        let full = "// before\r\nfn café() {\r\n    work();\r\n}\r\n// after\r\n";
        fs::write(&path, full).unwrap();
        let mut sources = Sources::new();
        let start = Position { line: 3, column: 1 };
        let end = Position { line: 4, column: 1 };
        let mut snippet = Snippet::new("café()".into(), &path, &start, "    work();\r\n", false);
        snippet.with_context(load(&mut sources, &path), &end);
        assert_eq!(&*snippet.display_source, full);
        assert_eq!(snippet.display_start_line, 1);
        assert_eq!((snippet.match_start_line, snippet.match_end_line), (3, 3));
        assert!(snippet.warning.is_none());
        // Source is read once for all occurrences in this browser snapshot.
        fs::remove_file(&path).unwrap();
        assert!(load(&mut sources, &path).is_ok());
        let large = dir.path().join("large.rs");
        fs::File::create(&large)
            .unwrap()
            .set_len(MAX_CONTEXT_BYTES + 1)
            .unwrap();
        let mut fallback = Snippet::new("a()".into(), &large, &start, "work();", false);
        fallback.with_context(load(&mut sources, &large), &end);
        assert_eq!(&*fallback.display_source, "work();");
        assert_eq!(fallback.display_start_line, 3);
        assert!(fallback.warning.as_deref().unwrap().contains("2 MiB"));
        let invalid = dir.path().join("binary.rs");
        fs::write(&invalid, [0xff, 0xfe]).unwrap();
        assert!(load(&mut sources, &invalid).is_err());
    }

    #[test]
    fn positions_are_unicode_aware_and_end_exclusive() {
        let source = "fn café() {\n    café();\n}\n";
        assert_eq!(
            slice(
                source,
                &Position { line: 2, column: 5 },
                &Position {
                    line: 2,
                    column: 12
                }
            ),
            Some("café();")
        );
        assert_eq!(offset(source, &Position { line: 0, column: 1 }), None);
        assert_eq!(
            offset(
                source,
                &Position {
                    line: 2,
                    column: 100
                }
            ),
            None
        );
        assert_eq!(
            offset(source, &Position { line: 4, column: 1 }),
            Some(source.len())
        );
    }

    #[test]
    fn semantic_snapshots_retain_source_and_remap_edges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        let source = "fn a() { work(); } fn b() { work(); }";
        fs::write(&path, source).unwrap();
        let report = semantic::analyze(
            vec![(path.clone(), source.into())],
            &SemanticOptions {
                min_tokens: 1,
                min_lines: 1,
                ..Default::default()
            },
            &mut Constant,
        )
        .unwrap();
        let browser = super::semantic(&report);
        assert_eq!(browser.groups.len(), 1);
        assert_eq!(browser.groups[0].occurrences[0].name, "a()");
        assert!(browser.groups[0].occurrences[0].warning.is_none());
        let occurrences = &browser.groups[0].occurrences;
        assert_eq!(&*occurrences[0].display_source, source);
        assert_eq!(occurrences[0].display_start_line, 1);
        assert!(Arc::ptr_eq(
            &occurrences[0].display_source,
            &occurrences[1].display_source
        ));
        assert_eq!(
            (
                browser.groups[0].edges[0].left,
                browser.groups[0].edges[0].right
            ),
            (0, 1)
        );
        fs::write(&path, "// changed").unwrap();
        let browser = super::semantic(&report);
        assert!(
            browser.groups[0].occurrences[0]
                .warning
                .as_deref()
                .unwrap()
                .contains("changed")
        );
        assert_eq!(browser.groups[0].occurrences[0].source, "{ work(); }");
        assert_eq!(
            &*browser.groups[0].occurrences[0].display_source,
            "{ work(); }"
        );
        fs::remove_file(&path).unwrap();
        assert!(
            super::semantic(&report).groups[0].occurrences[0]
                .warning
                .is_some()
        );
    }

    #[test]
    fn syntax_browser_reads_original_not_normalized_template() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        let source =
            "fn a() { let café = 42; consume(café); } fn b() { let value = 99; consume(value); }";
        fs::write(&path, source).unwrap();
        let report = rsdup::analyze_sources(
            vec![(path.clone(), source.into())],
            &Options {
                min_tokens: 5,
                min_lines: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let browser = syntax(&report);
        assert!(!browser.groups.is_empty());
        assert!(
            browser.groups[0]
                .occurrences
                .iter()
                .any(|snippet| snippet.source.contains("café"))
        );
        assert!(
            browser.groups[0]
                .occurrences
                .iter()
                .all(|snippet| snippet.warning.is_none())
        );
        fs::write(&path, source.replace("consume", "replace")).unwrap();
        let changed = syntax(&report);
        for snippet in &changed.groups[0].occurrences {
            assert!(snippet.warning.is_some());
            assert!(snippet.source.contains("consume"));
            assert!(!snippet.source.contains("replace"));
        }
        let json = serde_json::to_value(&report).unwrap();
        assert!(json["clones"][0]["occurrences"][0].get("source").is_none());
        fs::remove_file(path).unwrap();
        assert!(syntax(&report).groups[0].occurrences[0].warning.is_some());
    }
}
