//! Provider-independent semantic comparison of function, closure, and async bodies.
//!
//! No name resolution, macro expansion, or semantic-equivalence claim is made.
//! Comparisons take quadratic time; memory is linear in units plus accepted edges.

use std::{collections::BTreeMap, ops::Range, path::PathBuf};

use anyhow::{Context, Result, ensure};
use ra_ap_syntax::{AstNode, NodeOrToken, SyntaxKind, SyntaxNode, ast};
use serde::Serialize;

use crate::{Position, syntax};

#[derive(Clone, Debug, Serialize)]
pub struct SemanticOptions {
    pub min_tokens: usize,
    pub min_lines: usize,
    pub ignore_tests: bool,
    pub threshold: f64,
    /// Maximum Unicode scalar values in the comment-free embedding body.
    pub max_chars: usize,
}

impl Default for SemanticOptions {
    fn default() -> Self {
        Self {
            min_tokens: 40,
            min_lines: 5,
            ignore_tests: true,
            threshold: 0.93,
            max_chars: 6_000,
        }
    }
}

impl SemanticOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.min_tokens > 0, "min_tokens must be positive");
        ensure!(self.min_lines > 0, "min_lines must be positive");
        ensure!(self.max_chars > 0, "max_chars must be positive");
        ensure!(
            self.threshold.is_finite() && (-1.0..=1.0).contains(&self.threshold),
            "threshold must be finite and between -1 and 1"
        );
        Ok(())
    }
}

pub trait Embedder {
    fn embed(&mut self, body: &str) -> Result<Vec<f64>>;
}

#[derive(Clone, Debug, Serialize)]
pub struct SemanticReport {
    pub schema_version: u32,
    pub extraction_version: u32,
    pub body_identity: &'static str,
    pub overlap_policy: &'static str,
    pub clustering: &'static str,
    pub mode: String,
    pub policy: SemanticOptions,
    pub summary: SemanticSummary,
    pub units: Vec<SemanticUnit>,
    pub clusters: Vec<SemanticCluster>,
}

/// Counts refer to input entries and extracted bodies, not syntax statements.
/// Skip reasons are exclusive, in test/small/large priority order.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SemanticSummary {
    pub files: usize,
    pub units: usize,
    pub unique_bodies: usize,
    pub skipped_small: usize,
    pub skipped_large: usize,
    pub skipped_tests: usize,
    /// Units excluded from clusters by the greedy overlap policy, not from `units`.
    pub suppressed_overlaps: usize,
    pub clusters: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SemanticUnit {
    pub id: String,
    pub path: PathBuf,
    /// One-based line and Unicode-character column.
    pub start: Position,
    /// Exclusive end, using the same convention as `start`.
    pub end: Position,
    pub kind: String,
    pub context: String,
    pub test_context: bool,
    pub body_hash: String,
    /// BLAKE3 of the exact deduplicated representative sent to the embedder.
    pub embedding_input_hash: String,
    pub source: String,
    #[serde(skip)]
    pub embedding_body: String,
    pub tokens: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SemanticCluster {
    pub id: String,
    pub members: Vec<usize>,
    pub edges: Vec<SemanticEdge>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SemanticEdge {
    pub left: usize,
    pub right: usize,
    pub cosine: f64,
    /// Equality of trivia-free token hashes, not semantic equivalence.
    pub exact_body: bool,
}

/// Analyze all eligible bodies, including isolated units. The embedder is called
/// once per unique token hash, in hash order, using the lexicographically smallest
/// embedding body for that hash. Errors abort the entire report.
///
/// Input entries are sorted by path and source for deterministic traversal.
/// Paths are compared as supplied, not canonicalized. Duplicate entries retain
/// multiplicity. Test files are parsed too, so skipped-body counts stay precise.
pub fn analyze(
    mut sources: Vec<(PathBuf, String)>,
    options: &SemanticOptions,
    embedder: &mut impl Embedder,
) -> Result<SemanticReport> {
    options.validate()?;
    sources.sort();
    let mut summary = SemanticSummary {
        files: sources.len(),
        ..Default::default()
    };
    let mut units = Vec::new();
    let mut ranges = Vec::new();
    let mut occurrences = BTreeMap::<String, usize>::new();

    for (path, source) in sources {
        let tree = syntax::parse_source(&path, &source)?;
        let test_file = syntax::is_test_file(&path);
        let line_starts: Vec<_> = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        for owner in tree.syntax().descendants() {
            let Some((kind, body)) = body_of(&owner) else {
                continue;
            };
            let test_context = test_file
                || body
                    .ancestors()
                    .any(|node| syntax::has_test_attribute(&node))
                || syntax::contains_test_attribute(&body);
            if options.ignore_tests && test_context {
                summary.skipped_tests += 1;
                continue;
            }
            let range = byte_range(&body);
            let start = position(&source, &line_starts, range.start);
            let end = position(&source, &line_starts, range.end);
            let (body_hash, embedding_body, tokens) = body_text(&body);
            if tokens < options.min_tokens || end.line - start.line + 1 < options.min_lines {
                summary.skipped_small += 1;
                continue;
            }
            if embedding_body.chars().count() > options.max_chars {
                summary.skipped_large += 1;
                continue;
            }
            let mut identity = blake3::Hasher::new();
            // Encoded OS bytes avoid conflating distinct non-UTF-8 paths.
            frame(&mut identity, path.as_os_str().as_encoded_bytes());
            frame(&mut identity, kind.as_bytes());
            frame(&mut identity, body_hash.as_bytes());
            let base = identity.finalize().to_hex().to_string();
            let occurrence = occurrences.entry(base.clone()).or_default();
            let id = format!("{base}:{occurrence}");
            *occurrence += 1;
            units.push(SemanticUnit {
                id,
                path: path.clone(),
                start,
                end,
                kind: kind.to_owned(),
                context: context(&owner, &body),
                test_context,
                body_hash,
                embedding_input_hash: String::new(),
                source: source[range.clone()].to_owned(),
                embedding_body,
                tokens,
            });
            ranges.push(range);
        }
    }

    let mut representatives = BTreeMap::<&str, &str>::new();
    for unit in &units {
        representatives
            .entry(&unit.body_hash)
            .and_modify(|body| *body = (*body).min(unit.embedding_body.as_str()))
            .or_insert(&unit.embedding_body);
    }
    let mut vectors = BTreeMap::new();
    let mut input_hashes = BTreeMap::new();
    let mut dimension = None;
    for (hash, body) in representatives {
        let vector = embedder.embed(body).with_context(|| {
            let location = units
                .iter()
                .find(|unit| unit.body_hash == hash)
                .map(|unit| format!("{}:{}", unit.path.display(), unit.start.line))
                .unwrap_or_default();
            format!("embedding body {hash} at {location}")
        })?;
        let vector = normalize(vector, &mut dimension)
            .with_context(|| format!("invalid embedding for body {hash}"))?;
        vectors.insert(hash.to_owned(), vector);
        input_hashes.insert(
            hash.to_owned(),
            blake3::hash(body.as_bytes()).to_hex().to_string(),
        );
    }
    for unit in &mut units {
        unit.embedding_input_hash = input_hashes[&unit.body_hash].clone();
    }
    let mut edges = Vec::new();
    for left in 0..units.len() {
        for right in left + 1..units.len() {
            if units[left].path == units[right].path && overlaps(&ranges[left], &ranges[right]) {
                continue;
            }
            let a = &vectors[units[left].body_hash.as_str()];
            let b = &vectors[units[right].body_hash.as_str()];
            let exact_body = units[left].body_hash == units[right].body_hash;
            let cosine = if exact_body {
                1.0
            } else {
                compensated_sum(a.iter().zip(b).map(|(a, b)| a * b)).clamp(-1.0, 1.0)
            };
            if cosine >= options.threshold {
                edges.push(SemanticEdge {
                    left,
                    right,
                    cosine,
                    exact_body,
                });
            }
        }
    }
    summary.unique_bodies = vectors.len();
    let (clusters, suppressed_overlaps) = overlap_free_components(&units, &ranges, edges);
    summary.suppressed_overlaps = suppressed_overlaps;
    summary.units = units.len();
    summary.clusters = clusters.len();
    Ok(SemanticReport {
        schema_version: 1,
        extraction_version: 1,
        body_identity: "rust_tokens_v1",
        overlap_policy: "same_file_greedy_incident_cosine_desc_tokens_desc_id_asc_v1",
        clustering: "connected_components_of_retained_supporting_edges_v1",
        mode: "semantic".to_owned(),
        policy: options.clone(),
        summary,
        units,
        clusters,
    })
}

fn body_of(node: &SyntaxNode) -> Option<(&'static str, SyntaxNode)> {
    if let Some(function) = ast::Fn::cast(node.clone()) {
        return Some(("function", function.body()?.syntax().clone()));
    }
    if let Some(closure) = ast::ClosureExpr::cast(node.clone()) {
        return Some(("closure", closure.body()?.syntax().clone()));
    }
    if let Some(block) = ast::BlockExpr::cast(node.clone())
        && block.async_token().is_some()
    {
        return Some(("async", block.stmt_list()?.syntax().clone()));
    }
    None
}

fn byte_range(node: &SyntaxNode) -> Range<usize> {
    let range = node.text_range();
    usize::from(range.start())..usize::from(range.end())
}

fn position(source: &str, lines: &[usize], offset: usize) -> Position {
    let line = lines.partition_point(|start| *start <= offset) - 1;
    Position {
        line: line + 1,
        column: source[lines[line]..offset].chars().count() + 1,
    }
}

fn frame(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn body_text(body: &SyntaxNode) -> (String, String, usize) {
    let mut hash = blake3::Hasher::new();
    let mut text = String::new();
    let mut count = 0;
    for token in body
        .descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
    {
        match token.kind() {
            SyntaxKind::COMMENT => {
                // Preserve line breaks and token separation without touching literals.
                text.extend(
                    token
                        .text()
                        .chars()
                        .map(|c| if c == '\n' || c == '\r' { c } else { ' ' }),
                );
            }
            SyntaxKind::WHITESPACE => text.push_str(token.text()),
            kind => {
                frame(&mut hash, format!("{kind:?}").as_bytes());
                frame(&mut hash, token.text().as_bytes());
                text.push_str(token.text());
                count += 1;
            }
        }
    }
    (hash.finalize().to_hex().to_string(), text, count)
}

fn header(owner: &SyntaxNode, body: &SyntaxNode) -> String {
    let start = owner.text_range().start();
    let length = usize::from(body.text_range().start() - start);
    owner.text().to_string()[..length].trim().to_owned()
}

fn context(owner: &SyntaxNode, body: &SyntaxNode) -> String {
    let own = header(owner, body);
    if ast::Fn::can_cast(owner.kind()) {
        return own;
    }
    for ancestor in owner.ancestors().skip(1) {
        if let Some(function) = ast::Fn::cast(ancestor.clone())
            && let Some(body) = function.body()
        {
            return format!("{} :: {own}", header(&ancestor, body.syntax()));
        }
    }
    own
}

fn normalize(mut vector: Vec<f64>, dimension: &mut Option<usize>) -> Result<Vec<f64>> {
    ensure!(!vector.is_empty(), "vector must not be empty");
    ensure!(
        vector.iter().all(|value| value.is_finite()),
        "vector must contain only finite values"
    );
    if let Some(expected) = *dimension {
        ensure!(
            vector.len() == expected,
            "dimension mismatch: expected {expected}, got {}",
            vector.len()
        );
    }
    let scale = vector
        .iter()
        .fold(0.0_f64, |scale, value| scale.max(value.abs()));
    ensure!(scale > 0.0, "vector must have nonzero norm");
    for value in &mut vector {
        *value /= scale;
    }
    let norm = compensated_sum(vector.iter().map(|value| value * value)).sqrt();
    for value in &mut vector {
        *value /= norm;
    }
    *dimension = Some(vector.len());
    Ok(vector)
}

fn compensated_sum(values: impl Iterator<Item = f64>) -> f64 {
    let mut sum = 0.0;
    let mut correction = 0.0;
    for value in values {
        let adjusted = value - correction;
        let next = sum + adjusted;
        correction = (next - sum) - adjusted;
        sum = next;
    }
    sum
}

fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

/// Suppress overlaps within each initial component, then rebuild from surviving
/// edges: removing a bridging member must not leave a disconnected cluster.
fn overlap_free_components(
    units: &[SemanticUnit],
    ranges: &[Range<usize>],
    edges: Vec<SemanticEdge>,
) -> (Vec<SemanticCluster>, usize) {
    let mut strongest = vec![f64::NEG_INFINITY; units.len()];
    for edge in &edges {
        strongest[edge.left] = strongest[edge.left].max(edge.cosine);
        strongest[edge.right] = strongest[edge.right].max(edge.cosine);
    }
    let mut suppressed = vec![false; units.len()];
    let mut surviving_edges = Vec::new();
    for mut cluster in components(units, edges) {
        cluster.members.sort_by(|&left, &right| {
            strongest[right]
                .total_cmp(&strongest[left])
                .then_with(|| units[right].tokens.cmp(&units[left].tokens))
                .then_with(|| units[left].id.cmp(&units[right].id))
        });
        let mut retained: Vec<usize> = Vec::new();
        for member in cluster.members {
            if retained.iter().any(|&other| {
                units[member].path == units[other].path && overlaps(&ranges[member], &ranges[other])
            }) {
                suppressed[member] = true;
            } else {
                retained.push(member);
            }
        }
        surviving_edges.extend(
            cluster
                .edges
                .into_iter()
                .filter(|edge| !suppressed[edge.left] && !suppressed[edge.right]),
        );
    }
    surviving_edges.sort_by_key(|edge| (edge.left, edge.right));
    let count = suppressed.iter().filter(|&&value| value).count();
    (components(units, surviving_edges), count)
}

fn root(parents: &mut [usize], mut node: usize) -> usize {
    while parents[node] != node {
        parents[node] = parents[parents[node]];
        node = parents[node];
    }
    node
}

fn components(units: &[SemanticUnit], edges: Vec<SemanticEdge>) -> Vec<SemanticCluster> {
    let mut parents: Vec<_> = (0..units.len()).collect();
    for edge in &edges {
        let left = root(&mut parents, edge.left);
        let right = root(&mut parents, edge.right);
        parents[right.max(left)] = right.min(left);
    }
    let mut grouped = BTreeMap::<usize, SemanticCluster>::new();
    for edge in edges {
        let component = root(&mut parents, edge.left);
        grouped
            .entry(component)
            .or_insert_with(|| SemanticCluster {
                id: String::new(),
                members: Vec::new(),
                edges: Vec::new(),
            })
            .edges
            .push(edge);
    }
    for member in 0..units.len() {
        if let Some(cluster) = grouped.get_mut(&root(&mut parents, member)) {
            cluster.members.push(member);
        }
    }
    for cluster in grouped.values_mut() {
        let mut ids: Vec<_> = cluster
            .members
            .iter()
            .map(|&member| units[member].id.as_str())
            .collect();
        ids.sort_unstable();
        let mut hash = blake3::Hasher::new();
        for id in ids {
            frame(&mut hash, id.as_bytes());
        }
        cluster.id = hash.finalize().to_hex().to_string();
    }
    grouped.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Synthetic<F>(F);
    impl<F: FnMut(&str) -> Result<Vec<f64>>> Embedder for Synthetic<F> {
        fn embed(&mut self, body: &str) -> Result<Vec<f64>> {
            (self.0)(body)
        }
    }

    fn options() -> SemanticOptions {
        SemanticOptions {
            min_tokens: 1,
            min_lines: 1,
            ignore_tests: false,
            ..Default::default()
        }
    }

    fn run(source: &str) -> SemanticReport {
        analyze(
            vec![("src/lib.rs".into(), source.into())],
            &options(),
            &mut Synthetic(|_: &str| Ok(vec![1.0, 0.0])),
        )
        .unwrap()
    }

    #[test]
    fn extracts_only_bodies_with_headers_and_precise_spans() {
        let source = "fn outer() {\n    let é = |x| x + 1;\n    let b = || { call(); };\n    let c = async move { work().await; };\n    if true { ordinary(); }\n}";
        let report = run(source);
        assert_eq!(report.units.len(), 4);
        assert_eq!(
            report
                .units
                .iter()
                .map(|u| u.kind.as_str())
                .collect::<Vec<_>>(),
            ["function", "closure", "closure", "async"]
        );
        assert_eq!(report.units[0].context, "fn outer()");
        assert_eq!(report.units[1].context, "fn outer() :: |x|");
        assert_eq!(report.units[1].source, "x + 1");
        assert_eq!(
            report.units[1].start,
            Position {
                line: 2,
                column: 17
            }
        );
        assert_eq!(
            report.units[1].end,
            Position {
                line: 2,
                column: 22
            }
        );
        assert_eq!(report.units[2].source, "{ call(); }");
        assert_eq!(report.units[3].source, "{ work().await; }");
        assert!(
            report
                .clusters
                .iter()
                .flat_map(|c| &c.edges)
                .all(|e| e.left != 0)
        );
    }

    #[test]
    fn comments_are_removed_token_wise_but_literals_and_original_are_untouched() {
        let report = run(r##"fn a() { f/* comment */(r#"// literal  spaces"#); }
fn b() { f(r#"// literal  spaces"#); }
fn c() { f(r#"// literal spaces"#); }"##);
        assert!(report.units[0].source.contains("/* comment */"));
        assert!(!report.units[0].embedding_body.contains("comment"));
        assert!(
            report.units[0]
                .embedding_body
                .contains("// literal  spaces")
        );
        assert_eq!(report.units[0].body_hash, report.units[1].body_hash);
        assert_ne!(report.units[0].body_hash, report.units[2].body_hash);
        assert_eq!(report.summary.unique_bodies, 2);
        let edges = &report.clusters[0].edges;
        assert!(edges[0].exact_body);
        assert!(!edges[1].exact_body);
    }

    #[test]
    fn default_policy_excludes_tests_before_embedding() {
        let report = analyze(
            vec![(
                "tests/integration.rs".into(),
                "fn fixture() { work(); }".into(),
            )],
            &SemanticOptions::default(),
            &mut Synthetic(|_: &str| panic!("test bodies must not be embedded by default")),
        )
        .unwrap();
        assert!(report.policy.ignore_tests);
        assert_eq!(report.summary.skipped_tests, 1);
        assert!(report.units.is_empty());
    }

    #[test]
    fn nested_test_directories_are_excluded_unless_opted_in() {
        for path in [
            "src/adapters/test/test_input.rs",
            "src/adapters/tests/input.rs",
        ] {
            let sources = vec![(
                PathBuf::from(path),
                "fn a() { work(); } fn b() { work(); }".into(),
            )];
            let mut policy = SemanticOptions {
                min_tokens: 1,
                min_lines: 1,
                ..Default::default()
            };
            let excluded = analyze(
                sources.clone(),
                &policy,
                &mut Synthetic(|_: &str| panic!("test paths must not be embedded")),
            )
            .unwrap();
            assert_eq!(excluded.summary.skipped_tests, 2);
            assert!(excluded.units.is_empty());
            policy.ignore_tests = false;
            let included =
                analyze(sources, &policy, &mut Synthetic(|_: &str| Ok(vec![1.0]))).unwrap();
            assert_eq!(included.units.len(), 2);
            assert!(included.units.iter().all(|unit| unit.test_context));
            assert_eq!(included.clusters.len(), 1);
        }
    }

    #[test]
    fn test_ancestors_and_contaminated_enclosing_bodies_are_skipped() {
        let source = "#[cfg(not(test))] mod tests { fn a() { let f = || 1; } }
fn mixed() { #[test] fn inner() { work(); } }
fn clean() { ok(); }";
        let mut policy = options();
        policy.ignore_tests = true;
        let report = analyze(
            vec![("src/lib.rs".into(), source.into())],
            &policy,
            &mut Synthetic(|_: &str| Ok(vec![1.0])),
        )
        .unwrap();
        assert_eq!(report.summary.skipped_tests, 4);
        assert_eq!(report.units.len(), 1);
        assert_eq!(report.units[0].context, "fn clean()");
        let all = run(source);
        assert_eq!(all.units.iter().filter(|u| u.test_context).count(), 4);
        let report = analyze(
            vec![("tests/integration.rs".into(), "fn a() { ok(); }".into())],
            &policy,
            &mut Synthetic(|_: &str| panic!("test body must not be embedded")),
        )
        .unwrap();
        assert_eq!(report.summary.skipped_tests, 1);
        assert!(report.units.is_empty());
    }

    #[test]
    fn validates_vectors_and_scales_before_squaring() {
        for vector in [vec![], vec![0.0, -0.0], vec![f64::NAN], vec![f64::INFINITY]] {
            assert!(normalize(vector, &mut None).is_err());
        }
        assert!(normalize(vec![1.0], &mut Some(2)).is_err());
        for magnitude in [f64::MAX, f64::MIN_POSITIVE, f64::from_bits(1)] {
            let vector = normalize(vec![magnitude, magnitude], &mut None).unwrap();
            assert!(vector.iter().all(|v| v.is_finite()));
            assert!((vector[0] - 0.5_f64.sqrt()).abs() < 1e-15);
        }
        let result = analyze(
            vec![("a.rs".into(), "fn a() { a(); } fn b() { b(); }".into())],
            &options(),
            &mut Synthetic(|body: &str| {
                Ok(if body.contains("a()") {
                    vec![1.0]
                } else {
                    vec![1.0, 0.0]
                })
            }),
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("dimension mismatch"));
    }

    #[test]
    fn threshold_chain_keeps_supporting_edges_and_isolated_units() {
        let mut policy = options();
        policy.threshold = 0.7;
        let report = analyze(
            vec![(
                "a.rs".into(),
                "fn a() { a(); } fn b() { b(); } fn c() { c(); } fn d() { d(); }".into(),
            )],
            &policy,
            &mut Synthetic(|body: &str| {
                Ok(if body.contains("a()") {
                    vec![1.0, 0.0]
                } else if body.contains("b()") {
                    vec![1.0, 1.0]
                } else if body.contains("c()") {
                    vec![0.0, 1.0]
                } else {
                    vec![-1.0, -1.0]
                })
            }),
        )
        .unwrap();
        assert_eq!(report.units.len(), 4);
        assert_eq!(report.clusters.len(), 1);
        assert_eq!(report.clusters[0].members, [0, 1, 2]);
        assert_eq!(
            report.clusters[0]
                .edges
                .iter()
                .map(|e| (e.left, e.right))
                .collect::<Vec<_>>(),
            [(0, 1), (1, 2)]
        );
        policy.threshold = 0.0;
        let at_boundary = analyze(
            vec![("a.rs".into(), "fn a() { a(); } fn b() { b(); }".into())],
            &policy,
            &mut Synthetic(|body: &str| {
                Ok(if body.contains("a()") {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                })
            }),
        )
        .unwrap();
        assert_eq!(at_boundary.clusters[0].edges[0].cosine, 0.0);
    }

    #[test]
    fn overlap_filter_does_not_leave_a_phantom_component() {
        assert!(overlaps(&(0..5), &(3..8)));
        assert!(!overlaps(&(0..5), &(5..8)));
        let report = run("fn outer() { let f = || 1; }");
        assert_eq!(report.units.len(), 2);
        assert!(report.clusters.is_empty());
        let report = run("fn a() { a(); } fn b() { b(); } fn c() { c(); } fn d() { d(); }");
        let edge = |left, right| SemanticEdge {
            left,
            right,
            cosine: 1.0,
            exact_body: false,
        };
        let clusters = components(&report.units, vec![edge(0, 1), edge(2, 3)]);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].members, [0, 1]);
        assert_eq!(clusters[1].members, [2, 3]);
        assert_eq!(report.clusters[0].edges.len(), 6);
    }

    #[test]
    fn identical_bodies_embed_once_and_ids_preserve_multiplicity_not_lines() {
        let mut calls = 0;
        let mut embedder = Synthetic(|_: &str| {
            calls += 1;
            Ok(vec![1.0])
        });
        let report = analyze(
            vec![(
                "a.rs".into(),
                "fn a() { same(); } fn b() { same(); }".into(),
            )],
            &options(),
            &mut embedder,
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_ne!(report.units[0].id, report.units[1].id);
        let moved = analyze(
            vec![(
                "a.rs".into(),
                "\n\nfn a() { same(); }\nfn b() { same(); }".into(),
            )],
            &options(),
            &mut Synthetic(|_: &str| Ok(vec![1.0])),
        )
        .unwrap();
        assert_eq!(report.units[0].id, moved.units[0].id);
        assert_eq!(report.clusters[0].id, moved.clusters[0].id);
    }

    #[test]
    fn representative_is_lexicographic_and_exact_pairs_survive_threshold_one() {
        let sources = vec![
            (PathBuf::from("a.rs"), "fn a() { same(); }".to_owned()),
            (PathBuf::from("z.rs"), "fn z() {  same(); }".to_owned()),
        ];
        let policy = SemanticOptions {
            threshold: 1.0,
            ..options()
        };
        let mut inputs = Vec::new();
        let report = analyze(
            sources.clone(),
            &policy,
            &mut Synthetic(|body: &str| {
                inputs.push(body.to_owned());
                Ok(vec![1.0, 1.0])
            }),
        )
        .unwrap();
        assert_eq!(inputs, ["{  same(); }"]);
        let expected = blake3::hash(inputs[0].as_bytes()).to_hex().to_string();
        assert!(
            report
                .units
                .iter()
                .all(|unit| unit.embedding_input_hash == expected)
        );
        assert_ne!(report.units[0].embedding_body, inputs[0]);
        assert_eq!(report.clusters[0].edges[0].cosine, 1.0);
        assert!(report.clusters[0].edges[0].exact_body);
        let reversed = analyze(
            sources.into_iter().rev().collect(),
            &policy,
            &mut Synthetic(|_: &str| Ok(vec![1.0, 1.0])),
        )
        .unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json, serde_json::to_value(reversed).unwrap());
        assert_eq!(json["extraction_version"], 1);
        assert_eq!(json["body_identity"], "rust_tokens_v1");
        assert_eq!(json["overlap_policy"], report.overlap_policy);
        assert_eq!(json["clustering"], report.clustering);
        assert_eq!(json["units"][0]["embedding_input_hash"], expected);
    }

    #[test]
    fn external_bridge_cannot_cluster_overlapping_nested_bodies() {
        // A contains B; C is disjoint and has accepted edges to both.
        let report = run("fn a() { let b = || nested(); } fn c() { bridge(); }");
        assert_eq!(report.units.len(), 3);
        assert_eq!(report.summary.suppressed_overlaps, 1);
        assert_eq!(report.clusters.len(), 1);
        // Equal incident scores prefer the larger outer body.
        assert_eq!(report.clusters[0].members, [0, 2]);
        assert_eq!(report.clusters[0].edges.len(), 1);
        assert_eq!(
            (
                report.clusters[0].edges[0].left,
                report.clusters[0].edges[0].right
            ),
            (0, 2)
        );
        assert!(report.units[0].end.line <= report.units[2].start.line);
    }

    #[test]
    fn suppression_rebuilds_components_without_implied_edges() {
        let report =
            run("fn a() { a(); } fn b() { b(); } fn c() { c(); } fn d() { d(); } fn e() { e(); }");
        let ranges = [0..100, 10..20, 200..210, 300..310, 400..410];
        let edge = |left, right, cosine| SemanticEdge {
            left,
            right,
            cosine,
            exact_body: false,
        };
        let (clusters, count) = overlap_free_components(
            &report.units,
            &ranges,
            vec![
                edge(0, 2, 0.99),
                edge(1, 2, 0.9),
                edge(1, 3, 0.9),
                edge(3, 4, 0.95),
            ],
        );
        assert_eq!(count, 1);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].members, [0, 2]);
        assert_eq!(clusters[1].members, [3, 4]);
        for cluster in clusters {
            assert_eq!(cluster.edges.len(), 1);
            for (index, &left) in cluster.members.iter().enumerate() {
                for &right in &cluster.members[index + 1..] {
                    assert!(!overlaps(&ranges[left], &ranges[right]));
                }
            }
        }
    }

    #[test]
    fn overlap_priority_uses_score_then_tokens_then_id() {
        let mut units = run("fn a() { a(); } fn b() { b(); } fn c() { c(); }").units;
        let ranges = [0..100, 10..20, 200..210];
        units[0].id = "a".to_owned();
        units[1].id = "b".to_owned();
        for (score, tokens, expected) in [(0.99, 1, 1), (0.9, 100, 1), (0.9, 1, 0)] {
            units[0].tokens = 1;
            units[1].tokens = tokens;
            let (clusters, count) = overlap_free_components(
                &units,
                &ranges,
                vec![
                    SemanticEdge {
                        left: 0,
                        right: 2,
                        cosine: 0.9,
                        exact_body: false,
                    },
                    SemanticEdge {
                        left: 1,
                        right: 2,
                        cosine: score,
                        exact_body: false,
                    },
                ],
            );
            assert_eq!(count, 1);
            assert_eq!(clusters[0].members, [expected, 2]);
        }
    }

    #[test]
    fn options_and_size_limits_are_enforced_before_embedding() {
        for threshold in [f64::NAN, f64::INFINITY, -1.01, 1.01] {
            assert!(
                SemanticOptions {
                    threshold,
                    ..options()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            SemanticOptions {
                min_tokens: 0,
                ..options()
            }
            .validate()
            .is_err()
        );
        assert!(
            SemanticOptions {
                min_lines: 0,
                ..options()
            }
            .validate()
            .is_err()
        );
        assert!(
            SemanticOptions {
                max_chars: 0,
                ..options()
            }
            .validate()
            .is_err()
        );
        for policy in [
            SemanticOptions {
                min_tokens: 100,
                ..options()
            },
            SemanticOptions {
                min_lines: 2,
                ..options()
            },
            SemanticOptions {
                max_chars: 1,
                ..options()
            },
        ] {
            let report = analyze(
                vec![("a.rs".into(), "fn a() { ok(); }".into())],
                &policy,
                &mut Synthetic(|_: &str| panic!("ineligible body must not be embedded")),
            )
            .unwrap();
            assert_eq!(
                report.summary.skipped_small + report.summary.skipped_large,
                1
            );
            assert!(report.units.is_empty());
        }
        assert!(
            analyze(
                vec![("a.rs".into(), "fn broken(".into())],
                &options(),
                &mut Synthetic(|_: &str| Ok(vec![1.0]))
            )
            .is_err()
        );
    }
}
