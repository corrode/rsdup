use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use ra_ap_syntax::{
    AstNode, Edition, NodeOrToken, SourceFile, SyntaxKind, SyntaxNode, WalkEvent, ast,
};

use crate::model::Profile;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Part {
    Enter {
        kind: u16,
        range: (usize, usize),
        hole_eligible: bool,
    },
    Leave(u16),
    Exact(u16, String),
    Local {
        name: String,
        range: (usize, usize),
    },
    Literal {
        kind: u16,
        shape: String,
        range: (usize, usize),
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Unit {
    pub range: (usize, usize),
    pub rough: Vec<String>,
    pub near_key: Vec<String>,
    pub structure: Vec<Part>,
    tree: StructuralNode,
    pub tokens: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Container {
    pub file_id: usize,
    pub units: Vec<Unit>,
    pub rough: Vec<String>,
    pub unit_token_ranges: Vec<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedFile {
    pub path: PathBuf,
    pub source: String,
    pub line_starts: Vec<usize>,
}

#[derive(Clone, Debug)]
struct LocalBinding {
    declaration: (usize, usize),
    scope: (usize, usize),
}

#[derive(Clone, Debug, Default)]
struct LocalBindings {
    by_name: HashMap<String, LocalBinding>,
    declarations: HashMap<(usize, usize), String>,
}

pub(crate) fn parse_sources(
    sources: Vec<(PathBuf, String)>,
    profile: Profile,
    ignore_tests: bool,
) -> Result<(Vec<ParsedFile>, Vec<Container>)> {
    let mut files = Vec::with_capacity(sources.len());
    let mut containers = Vec::new();

    for (path, source) in sources {
        if ignore_tests && is_test_file(&path) {
            continue;
        }

        let file_id = files.len();
        let tree = parse_source(&path, &source)?;
        for block in tree.syntax().descendants().filter_map(ast::BlockExpr::cast) {
            if ignore_tests && is_test_block(&block) {
                continue;
            }
            if let Some(container) = build_container(file_id, &block, profile, ignore_tests) {
                containers.push(container);
            }
        }

        files.push(ParsedFile {
            path,
            line_starts: line_starts(&source),
            source,
        });
    }

    Ok((files, containers))
}

pub(crate) fn is_test_file(path: &Path) -> bool {
    let in_tests_directory = path
        .iter()
        .any(|component| component == "test" || component == "tests");
    let test_filename = path.file_stem().is_some_and(|stem| {
        let stem = stem.to_string_lossy();
        stem == "test" || stem == "tests" || stem.ends_with("_test")
    });
    in_tests_directory || test_filename
}

fn is_test_block(block: &ast::BlockExpr) -> bool {
    block
        .syntax()
        .ancestors()
        .any(|node| has_test_attribute(&node))
}

pub(crate) fn has_test_attribute(node: &SyntaxNode) -> bool {
    node.children()
        .filter(|child| child.kind() == SyntaxKind::ATTR)
        .any(|attribute| is_test_attribute(&attribute))
}

pub(crate) fn contains_test_attribute(node: &SyntaxNode) -> bool {
    node.descendants()
        .filter(|descendant| descendant.kind() == SyntaxKind::ATTR)
        .any(|attribute| is_test_attribute(&attribute))
}

fn is_test_attribute(attribute: &SyntaxNode) -> bool {
    let compact: String = attribute
        .text()
        .to_string()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();

    if compact.starts_with("#[cfg(") {
        return attribute
            .descendants_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .any(|token| token.kind() == SyntaxKind::IDENT && token.text() == "test");
    }

    let Some(body) = compact
        .strip_prefix("#[")
        .and_then(|body| body.strip_suffix(']'))
    else {
        return false;
    };
    let path = body.split(['(', '=']).next().unwrap_or(body);
    let leaf = path.rsplit("::").next().unwrap_or(path);
    matches!(
        leaf,
        "test" | "rstest" | "test_case" | "wasm_bindgen_test" | "quickcheck" | "proptest"
    ) || leaf.ends_with("_test")
}

pub(crate) fn parse_source(path: &Path, source: &str) -> Result<ast::SourceFile> {
    let mut best_errors = Vec::new();
    for edition in [
        Edition::Edition2024,
        Edition::Edition2021,
        Edition::Edition2018,
        Edition::Edition2015,
    ] {
        let parse = SourceFile::parse(source, edition);
        let errors = parse.errors();
        if errors.is_empty() {
            return Ok(parse.tree());
        }
        if best_errors.is_empty() || errors.len() < best_errors.len() {
            best_errors = errors;
        }
    }

    let details = best_errors
        .iter()
        .take(3)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    bail!("failed to parse {}: {details}", path.display())
}

fn build_container(
    file_id: usize,
    block: &ast::BlockExpr,
    profile: Profile,
    ignore_tests: bool,
) -> Option<Container> {
    let local_bindings = local_bindings(block.syntax());
    let mut nodes: Vec<SyntaxNode> = block
        .statements()
        .filter_map(|statement| match statement {
            ast::Stmt::ExprStmt(_) | ast::Stmt::LetStmt(_) => Some(statement.syntax().clone()),
            ast::Stmt::Item(_) => None,
        })
        .filter(|node| !ignore_tests || !contains_test_attribute(node))
        .collect();
    if let Some(tail) = block.tail_expr()
        && (!ignore_tests || !contains_test_attribute(tail.syntax()))
    {
        nodes.push(tail.syntax().clone());
    }
    if nodes.is_empty() {
        return None;
    }

    let units: Vec<_> = nodes
        .iter()
        .filter_map(|node| build_unit(node, &local_bindings, profile))
        .collect();
    if units.is_empty() {
        return None;
    }

    let mut rough = Vec::new();
    let mut unit_token_ranges = Vec::with_capacity(units.len());
    for unit in &units {
        let start = rough.len();
        rough.extend(unit.rough.iter().cloned());
        unit_token_ranges.push((start, rough.len()));
    }

    Some(Container {
        file_id,
        units,
        rough,
        unit_token_ranges,
    })
}

fn build_unit(root: &SyntaxNode, local_bindings: &LocalBindings, profile: Profile) -> Option<Unit> {
    let mut structure = Vec::new();
    let mut rough = Vec::new();

    for event in root.preorder_with_tokens() {
        match event {
            WalkEvent::Enter(NodeOrToken::Node(node)) => {
                structure.push(Part::Enter {
                    kind: kind_id(node.kind()),
                    range: text_range(node.text_range()),
                    hole_eligible: is_hole_eligible(&node, root),
                });
            }
            WalkEvent::Leave(NodeOrToken::Node(node)) => {
                structure.push(Part::Leave(kind_id(node.kind())));
            }
            WalkEvent::Enter(NodeOrToken::Token(token)) => {
                if is_trivia(token.kind()) {
                    continue;
                }
                let part = normalize_token(&token, local_bindings, profile);
                rough.push(rough_key(&part));
                structure.push(part);
            }
            WalkEvent::Leave(NodeOrToken::Token(_)) => {}
        }
    }

    if rough.is_empty() {
        return None;
    }
    let range = root.text_range();
    let near_key = near_key(&structure);
    let tree = structural_tree(&structure)?;
    Some(Unit {
        range: (
            u32::from(range.start()) as usize,
            u32::from(range.end()) as usize,
        ),
        tokens: rough.len(),
        rough,
        near_key,
        structure,
        tree,
    })
}

fn is_hole_eligible(node: &SyntaxNode, unit_root: &SyntaxNode) -> bool {
    if node == unit_root || !ast::Expr::can_cast(node.kind()) || is_call_callee(node) {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    if matches!(
        parent.kind(),
        SyntaxKind::EXPR_STMT | SyntaxKind::LET_STMT | SyntaxKind::CALL_EXPR
    ) {
        return false;
    }

    !node.descendants().any(|descendant| {
        matches!(
            descendant.kind(),
            SyntaxKind::ATTR
                | SyntaxKind::TOKEN_TREE
                | SyntaxKind::TOKEN_TREE_META
                | SyntaxKind::BLOCK_EXPR
                | SyntaxKind::CALL_EXPR
                | SyntaxKind::METHOD_CALL_EXPR
                | SyntaxKind::AWAIT_EXPR
                | SyntaxKind::TRY_EXPR
                | SyntaxKind::RETURN_EXPR
                | SyntaxKind::BREAK_EXPR
                | SyntaxKind::CONTINUE_EXPR
                | SyntaxKind::YIELD_EXPR
        )
    }) && !node
        .descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| {
            matches!(
                token.text(),
                "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>="
            )
        })
        && !node.ancestors().any(|ancestor| {
            matches!(
                ancestor.kind(),
                SyntaxKind::ATTR | SyntaxKind::TOKEN_TREE | SyntaxKind::TOKEN_TREE_META
            )
        })
}

fn is_call_callee(node: &SyntaxNode) -> bool {
    for ancestor in node.ancestors().skip(1) {
        match ancestor.kind() {
            SyntaxKind::ARG_LIST => return false,
            SyntaxKind::CALL_EXPR => return true,
            SyntaxKind::EXPR_STMT | SyntaxKind::LET_STMT => return false,
            _ => {}
        }
    }
    false
}

fn near_key(parts: &[Part]) -> Vec<String> {
    let mut result = Vec::new();
    let mut skipped_depth = 0_usize;

    for part in parts {
        match part {
            Part::Enter { hole_eligible, .. } if skipped_depth == 0 && *hole_eligible => {
                result.push("expression-hole".to_owned());
                skipped_depth = 1;
            }
            Part::Enter { kind, .. } if skipped_depth == 0 => {
                result.push(format!("enter:{kind}"));
            }
            Part::Enter { .. } => skipped_depth += 1,
            Part::Leave(_) if skipped_depth > 0 => skipped_depth -= 1,
            Part::Leave(kind) => result.push(format!("leave:{kind}")),
            part if skipped_depth == 0 => result.push(rough_key(part)),
            Part::Exact(_, _) | Part::Local { .. } | Part::Literal { .. } => {}
        }
    }
    result
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StructuralNode {
    kind: u16,
    range: (usize, usize),
    hole_eligible: bool,
    children: Vec<StructuralElement>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StructuralElement {
    Node(StructuralNode),
    Exact(u16, String),
    Local(String),
    Literal(u16, String),
}

fn structural_tree(parts: &[Part]) -> Option<StructuralNode> {
    let mut index = 0;
    let tree = parse_structural_node(parts, &mut index)?;
    (index == parts.len()).then_some(tree)
}

fn parse_structural_node(parts: &[Part], index: &mut usize) -> Option<StructuralNode> {
    let Part::Enter {
        kind,
        range,
        hole_eligible,
    } = parts.get(*index)?
    else {
        return None;
    };
    let (kind, range, hole_eligible) = (*kind, *range, *hole_eligible);
    *index += 1;
    let mut children = Vec::new();

    loop {
        match parts.get(*index)? {
            Part::Enter { .. } => {
                children.push(StructuralElement::Node(parse_structural_node(
                    parts, index,
                )?));
            }
            Part::Leave(leave_kind) if *leave_kind == kind => {
                *index += 1;
                break;
            }
            Part::Leave(_) => return None,
            Part::Exact(token_kind, text) => {
                children.push(StructuralElement::Exact(*token_kind, text.clone()));
                *index += 1;
            }
            Part::Local { name, .. } => {
                children.push(StructuralElement::Local(name.clone()));
                *index += 1;
            }
            Part::Literal {
                kind: token_kind,
                shape,
                ..
            } => {
                children.push(StructuralElement::Literal(*token_kind, shape.clone()));
                *index += 1;
            }
        }
    }

    Some(StructuralNode {
        kind,
        range,
        hole_eligible,
        children,
    })
}

fn local_bindings(block: &SyntaxNode) -> LocalBindings {
    let root = block
        .ancestors()
        .find(|node| matches!(node.kind(), SyntaxKind::FN | SyntaxKind::CLOSURE_EXPR))
        .unwrap_or_else(|| block.clone());
    let mut candidates = Vec::new();
    let mut counts = HashMap::new();

    for pattern in root
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::IDENT_PAT)
    {
        let Some(token) = pattern
            .descendants_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .find(|token| token.kind() == SyntaxKind::IDENT)
        else {
            continue;
        };
        let Some(scope) = binding_scope(&pattern) else {
            continue;
        };
        let name = token.text().to_owned();
        *counts.entry(name.clone()).or_insert(0) += 1;
        candidates.push((name, text_range(token.text_range()), scope));
    }

    let mut bindings = LocalBindings::default();
    for (name, declaration, scope) in candidates {
        // Shadowing needs full name resolution. Preserve every ambiguous spelling
        // rather than risk equating references to different bindings.
        if counts.get(&name) != Some(&1) {
            continue;
        }
        bindings.declarations.insert(declaration, name.clone());
        bindings
            .by_name
            .insert(name, LocalBinding { declaration, scope });
    }
    bindings
}

fn binding_scope(pattern: &SyntaxNode) -> Option<(usize, usize)> {
    for ancestor in pattern.ancestors() {
        match ancestor.kind() {
            SyntaxKind::LET_STMT => {
                let statement = text_range(ancestor.text_range());
                let list = ancestor
                    .parent()
                    .map(|node| text_range(node.text_range()))?;
                return Some((statement.1, list.1));
            }
            SyntaxKind::PARAM => {
                let owner = ancestor.ancestors().find(|node| {
                    matches!(node.kind(), SyntaxKind::FN | SyntaxKind::CLOSURE_EXPR)
                })?;
                return Some((
                    text_range(pattern.text_range()).1,
                    text_range(owner.text_range()).1,
                ));
            }
            SyntaxKind::FOR_EXPR => {
                return body_scope(&ancestor, pattern);
            }
            SyntaxKind::MATCH_ARM => {
                let arm = text_range(ancestor.text_range());
                return Some((text_range(pattern.text_range()).1, arm.1));
            }
            SyntaxKind::LET_EXPR => {
                let owner = ancestor.ancestors().find(|node| {
                    matches!(node.kind(), SyntaxKind::IF_EXPR | SyntaxKind::WHILE_EXPR)
                })?;
                return body_scope(&owner, &ancestor);
            }
            SyntaxKind::CLOSURE_EXPR => {
                let closure = text_range(ancestor.text_range());
                return Some((text_range(pattern.text_range()).1, closure.1));
            }
            _ => {}
        }
    }
    None
}

fn body_scope(owner: &SyntaxNode, after: &SyntaxNode) -> Option<(usize, usize)> {
    let after_end = text_range(after.text_range()).1;
    owner
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::BLOCK_EXPR)
        .map(|node| text_range(node.text_range()))
        .find(|range| range.0 >= after_end)
}

fn normalize_token(
    token: &ra_ap_syntax::SyntaxToken,
    local_bindings: &LocalBindings,
    profile: Profile,
) -> Part {
    let kind = kind_id(token.kind());
    let text = token.text().to_owned();

    if profile == Profile::Strict || is_opaque_macro_input(token) {
        return Part::Exact(kind, text);
    }

    if token.kind() == SyntaxKind::IDENT
        && local_bindings.by_name.contains_key(&text)
        && is_local_identifier(token, local_bindings)
    {
        return Part::Local {
            name: text,
            range: text_range(token.text_range()),
        };
    }

    if is_literal(token.kind()) {
        return Part::Literal {
            kind,
            shape: literal_shape(token.kind(), &text),
            range: text_range(token.text_range()),
        };
    }

    Part::Exact(kind, text)
}

fn is_local_identifier(token: &ra_ap_syntax::SyntaxToken, local_bindings: &LocalBindings) -> bool {
    let range = text_range(token.text_range());
    if local_bindings.declarations.contains_key(&range) {
        return true;
    }

    let Some(binding) = local_bindings.by_name.get(token.text()) else {
        return false;
    };
    if range.0 < binding.scope.0 || range.1 > binding.scope.1 || range == binding.declaration {
        return false;
    }

    let Some(parent) = token.parent() else {
        return false;
    };
    let Some(path_expr) = parent
        .ancestors()
        .find(|node| node.kind() == SyntaxKind::PATH_EXPR)
    else {
        return false;
    };
    path_expr
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::NAME_REF)
        .count()
        == 1
}

fn is_opaque_macro_input(token: &ra_ap_syntax::SyntaxToken) -> bool {
    token.parent().is_some_and(|parent| {
        parent.ancestors().any(|ancestor| {
            matches!(
                ancestor.kind(),
                SyntaxKind::TOKEN_TREE | SyntaxKind::TOKEN_TREE_META | SyntaxKind::ATTR
            )
        })
    })
}

fn text_range(range: ra_ap_syntax::TextRange) -> (usize, usize) {
    (
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn literal_shape(kind: SyntaxKind, text: &str) -> String {
    match kind {
        SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER => {
            const SUFFIXES: [&str; 14] = [
                "isize", "usize", "i128", "u128", "i64", "u64", "f64", "i32", "u32", "f32", "i16",
                "u16", "i8", "u8",
            ];
            let suffix = SUFFIXES
                .into_iter()
                .find(|suffix| {
                    text.strip_suffix(suffix).is_some_and(|number| {
                        !number.is_empty()
                            && !matches!(number, "0x" | "0o" | "0b")
                            && number.chars().any(|character| character.is_ascii_digit())
                    })
                })
                .unwrap_or("");
            format!("number:{suffix}")
        }
        SyntaxKind::BYTE => "byte".to_owned(),
        SyntaxKind::BYTE_STRING => "byte-string".to_owned(),
        SyntaxKind::CHAR => "char".to_owned(),
        SyntaxKind::C_STRING => "c-string".to_owned(),
        SyntaxKind::STRING => "string".to_owned(),
        _ => "literal".to_owned(),
    }
}

fn rough_key(part: &Part) -> String {
    match part {
        Part::Local { .. } => "local".to_owned(),
        Part::Literal { kind, shape, .. } => format!("literal:{kind}:{shape}"),
        Part::Exact(kind, text) => format!("token:{kind}:{text}"),
        Part::Enter { .. } | Part::Leave(_) => unreachable!("nodes are not lexical tokens"),
    }
}

fn is_trivia(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::WHITESPACE | SyntaxKind::COMMENT | SyntaxKind::NEWLINE
    )
}

fn is_literal(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::INT_NUMBER
            | SyntaxKind::FLOAT_NUMBER
            | SyntaxKind::BYTE
            | SyntaxKind::BYTE_STRING
            | SyntaxKind::CHAR
            | SyntaxKind::C_STRING
            | SyntaxKind::STRING
    )
}

fn kind_id(kind: SyntaxKind) -> u16 {
    kind as u16
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(source.match_indices('\n').map(|(index, _)| index + 1));
    starts
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HoleRange {
    pub id: usize,
    pub range: (usize, usize),
}

#[derive(Clone, Copy)]
enum PlaceholderSource<'a> {
    Local(&'a str),
    Literal,
    Hole(usize),
}

pub(crate) fn syntax_template(units: &[Unit], source: &str, holes: &[HoleRange]) -> String {
    let Some(first) = units.first() else {
        return String::new();
    };
    let Some(last) = units.last() else {
        return String::new();
    };

    let mut replacements = Vec::new();
    for part in units.iter().flat_map(|unit| &unit.structure) {
        match part {
            Part::Local { name, range } => {
                replacements.push((*range, PlaceholderSource::Local(name)));
            }
            Part::Literal { range, .. } => {
                replacements.push((*range, PlaceholderSource::Literal));
            }
            Part::Enter { .. } | Part::Leave(_) | Part::Exact(_, _) => {}
        }
    }
    replacements.extend(
        holes
            .iter()
            .map(|hole| (hole.range, PlaceholderSource::Hole(hole.id))),
    );
    replacements.sort_by(|left, right| {
        left.0
            .0
            .cmp(&right.0.0)
            .then_with(|| right.0.1.cmp(&left.0.1))
    });

    let start = first.range.0;
    let end = last.range.1;
    let mut cursor = start;
    let mut template = String::new();
    let mut local_placeholders = HashMap::<&str, usize>::new();
    let mut hole_placeholders = HashMap::<usize, usize>::new();
    let mut next_placeholder = 1;
    for ((replacement_start, replacement_end), source_kind) in replacements {
        if replacement_start < cursor || replacement_end > end {
            continue;
        }
        let placeholder = match source_kind {
            PlaceholderSource::Local(name) => {
                let next = next_placeholder;
                *local_placeholders.entry(name).or_insert_with(|| {
                    next_placeholder += 1;
                    next
                })
            }
            PlaceholderSource::Literal => {
                let placeholder = next_placeholder;
                next_placeholder += 1;
                placeholder
            }
            PlaceholderSource::Hole(id) => {
                let next = next_placeholder;
                *hole_placeholders.entry(id).or_insert_with(|| {
                    next_placeholder += 1;
                    next
                })
            }
        };
        template.push_str(&source[cursor..replacement_start]);
        template.push('$');
        template.push_str(&placeholder.to_string());
        cursor = replacement_end;
    }
    template.push_str(&source[cursor..end]);

    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
    let indentation = &source[line_start..start];
    if !indentation.is_empty() && indentation.chars().all(char::is_whitespace) {
        template = template.replace(&format!("\n{indentation}"), "\n");
    }
    template
}

pub(crate) fn canonical_structure(units: &[Unit]) -> Vec<CanonicalPart> {
    let mut locals = HashMap::<&str, usize>::new();
    let mut result = Vec::new();

    for part in units.iter().flat_map(|unit| &unit.structure) {
        match part {
            Part::Enter { kind, .. } => result.push(CanonicalPart::Enter(*kind)),
            Part::Leave(kind) => result.push(CanonicalPart::Leave(*kind)),
            Part::Exact(kind, text) => result.push(CanonicalPart::Token(*kind, text.clone())),
            Part::Literal { kind, shape, .. } => {
                result.push(CanonicalPart::Token(*kind, shape.clone()));
            }
            Part::Local { name, .. } => {
                let next = locals.len();
                let id = *locals.entry(name).or_insert(next);
                result.push(CanonicalPart::Local(id));
            }
        }
    }
    result
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum CanonicalPart {
    Enter(u16),
    Leave(u16),
    Token(u16, String),
    Local(usize),
    Hole(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GeneralizedMatch {
    pub canonical: Vec<CanonicalPart>,
    pub left_holes: Vec<HoleRange>,
    pub right_holes: Vec<HoleRange>,
    pub anchor_tokens: usize,
}

#[derive(Clone, Debug, Default)]
struct MatchState {
    canonical: Vec<CanonicalPart>,
    left_holes: Vec<HoleRange>,
    right_holes: Vec<HoleRange>,
    left_locals: HashMap<String, usize>,
    right_locals: HashMap<String, usize>,
    anchor_tokens: usize,
}

pub(crate) fn generalize_units(
    left: &[Unit],
    right: &[Unit],
    max_holes: usize,
) -> Option<GeneralizedMatch> {
    if left.len() != right.len() {
        return None;
    }

    let mut state = MatchState::default();
    for (left, right) in left.iter().zip(right) {
        match_nodes(&left.tree, &right.tree, &mut state).ok()?;
    }
    if state.left_holes.len() > max_holes {
        return None;
    }

    Some(GeneralizedMatch {
        canonical: state.canonical,
        left_holes: state.left_holes,
        right_holes: state.right_holes,
        anchor_tokens: state.anchor_tokens,
    })
}

fn match_nodes(
    left: &StructuralNode,
    right: &StructuralNode,
    state: &mut MatchState,
) -> Result<(), ()> {
    let checkpoint = state.clone();
    if left.kind == right.kind && left.children.len() == right.children.len() {
        state.canonical.push(CanonicalPart::Enter(left.kind));
        let children_match = left
            .children
            .iter()
            .zip(&right.children)
            .try_for_each(|(left, right)| match_elements(left, right, state));
        if children_match.is_ok() {
            state.canonical.push(CanonicalPart::Leave(left.kind));
            return Ok(());
        }
    }

    *state = checkpoint;
    if !left.hole_eligible || !right.hole_eligible {
        return Err(());
    }
    let id = state.left_holes.len();
    state.canonical.push(CanonicalPart::Hole(id));
    state.left_holes.push(HoleRange {
        id,
        range: left.range,
    });
    state.right_holes.push(HoleRange {
        id,
        range: right.range,
    });
    Ok(())
}

fn match_elements(
    left: &StructuralElement,
    right: &StructuralElement,
    state: &mut MatchState,
) -> Result<(), ()> {
    match (left, right) {
        (StructuralElement::Node(left), StructuralElement::Node(right)) => {
            match_nodes(left, right, state)
        }
        (
            StructuralElement::Exact(left_kind, left),
            StructuralElement::Exact(right_kind, right),
        ) if left_kind == right_kind && left == right => {
            state
                .canonical
                .push(CanonicalPart::Token(*left_kind, left.clone()));
            state.anchor_tokens += 1;
            Ok(())
        }
        (
            StructuralElement::Literal(left_kind, left),
            StructuralElement::Literal(right_kind, right),
        ) if left_kind == right_kind && left == right => {
            state
                .canonical
                .push(CanonicalPart::Token(*left_kind, left.clone()));
            state.anchor_tokens += 1;
            Ok(())
        }
        (StructuralElement::Local(left), StructuralElement::Local(right)) => {
            let left_id = local_id(&mut state.left_locals, left);
            let right_id = local_id(&mut state.right_locals, right);
            if left_id != right_id {
                return Err(());
            }
            state.canonical.push(CanonicalPart::Local(left_id));
            state.anchor_tokens += 1;
            Ok(())
        }
        _ => Err(()),
    }
}

fn local_id(locals: &mut HashMap<String, usize>, name: &str) -> usize {
    let next = locals.len();
    *locals.entry(name.to_owned()).or_insert(next)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{canonical_structure, parse_sources};
    use crate::model::Profile;

    fn structures(source: &str, profile: Profile) -> Vec<Vec<super::CanonicalPart>> {
        let (_, containers) = parse_sources(
            vec![(PathBuf::from("input.rs"), source.to_owned())],
            profile,
            false,
        )
        .unwrap();
        containers
            .iter()
            .map(|container| canonical_structure(&container.units))
            .collect()
    }

    #[test]
    fn balanced_profile_normalizes_unambiguous_locals() {
        let left = structures(
            "fn f(value: i32) { let doubled = value * 2; doubled + 1 }",
            Profile::Balanced,
        );
        let right = structures(
            "fn g(input: i32) { let result = input * 9; result + 7 }",
            Profile::Balanced,
        );
        assert_eq!(left, right);
    }

    #[test]
    fn strict_profile_preserves_names_and_literals() {
        let left = structures("fn f(value: i32) { value + 1 }", Profile::Strict);
        let right = structures("fn g(input: i32) { input + 2 }", Profile::Strict);
        assert_ne!(left, right);
    }

    #[test]
    fn method_names_remain_semantic_anchors() {
        let left = structures("fn f(value: T) { value.send(1); }", Profile::Balanced);
        let right = structures("fn f(value: T) { value.store(2); }", Profile::Balanced);
        assert_ne!(left, right);
    }
}
