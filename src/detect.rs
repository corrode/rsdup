use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet},
};

use crate::{
    model::{CloneClass, DetectionMode, Occurrence, Options, Position, Report, SortOrder, Summary},
    syntax::{
        CanonicalPart, Container, HoleRange, ParsedFile, canonical_structure, generalize_units,
        syntax_template as render_syntax_template,
    },
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Posting {
    container: usize,
    token: usize,
}

type SourceRange = (usize, usize, usize);

#[derive(Debug)]
struct RawClass {
    canonical: Vec<CanonicalPart>,
    tokens: usize,
    holes: usize,
    occurrences: BTreeSet<SourceRange>,
    hole_ranges: HashMap<SourceRange, Vec<HoleRange>>,
}

struct VerifiedMatch {
    canonical: Vec<CanonicalPart>,
    tokens: usize,
    left: SourceRange,
    right: SourceRange,
    left_holes: Vec<HoleRange>,
    right_holes: Vec<HoleRange>,
}

pub(crate) fn detect(files: &[ParsedFile], containers: &[Container], options: &Options) -> Report {
    if options.min_tokens == 0 {
        return empty_report(files.len());
    }

    let mut report = match options.mode {
        DetectionMode::Exact => detect_exact(files, containers, options),
        DetectionMode::NearMiss => detect_near_misses(files, containers, options),
    };
    // Snapshot only retained occurrences, not the much larger candidate set.
    for occurrence in report
        .clones
        .iter_mut()
        .flat_map(|clone| &mut clone.occurrences)
    {
        occurrence.source =
            files[occurrence.file_id].source[occurrence.byte_start..occurrence.byte_end].to_owned();
    }
    report
}

fn detect_exact(files: &[ParsedFile], containers: &[Container], options: &Options) -> Report {
    let mut index: HashMap<u64, Vec<Posting>> = HashMap::new();
    for (container_id, container) in containers.iter().enumerate() {
        for (hash, token) in fingerprints(&container.rough, options.min_tokens) {
            index.entry(hash).or_default().push(Posting {
                container: container_id,
                token,
            });
        }
    }

    let mut seen_seeds = HashSet::new();
    let mut raw_classes: HashMap<Vec<CanonicalPart>, RawClass> = HashMap::new();

    for postings in index.into_values() {
        if postings.len() < 2 {
            continue;
        }
        for left_index in 0..postings.len() {
            for right_index in left_index + 1..postings.len() {
                let mut first = postings[left_index];
                let mut second = postings[right_index];
                if second < first {
                    std::mem::swap(&mut first, &mut second);
                }
                if !seen_seeds.insert((first, second)) {
                    continue;
                }
                for found in verify_pair(first, second, containers, files, options) {
                    insert_verified(&mut raw_classes, found);
                }
            }
        }
    }

    finish_report(raw_classes, files, containers, options)
}

fn detect_near_misses(files: &[ParsedFile], containers: &[Container], options: &Options) -> Report {
    let mut index: HashMap<Vec<String>, Vec<Posting>> = HashMap::new();
    for (container_id, container) in containers.iter().enumerate() {
        for (unit, statement) in container.units.iter().enumerate() {
            index
                .entry(statement.near_key.clone())
                .or_default()
                .push(Posting {
                    container: container_id,
                    token: unit,
                });
        }
    }

    let mut seen_starts = HashSet::new();
    let mut raw_classes = HashMap::new();
    for postings in index.into_values() {
        if postings.len() < 2 {
            continue;
        }
        for left_index in 0..postings.len() {
            for right_index in left_index + 1..postings.len() {
                let mut first = postings[left_index];
                let mut second = postings[right_index];
                if second < first {
                    std::mem::swap(&mut first, &mut second);
                }
                if !seen_starts.insert((first, second)) {
                    continue;
                }
                if let Some(found) = verify_near_pair(first, second, containers, files, options) {
                    insert_verified(&mut raw_classes, found);
                }
            }
        }
    }

    finish_report(raw_classes, files, containers, options)
}

fn verify_near_pair(
    first: Posting,
    second: Posting,
    containers: &[Container],
    files: &[ParsedFile],
    options: &Options,
) -> Option<VerifiedMatch> {
    if first == second {
        return None;
    }
    let left = &containers[first.container];
    let right = &containers[second.container];
    if left.units[first.token].near_key != right.units[second.token].near_key {
        return None;
    }

    let max_length = (left.units.len() - first.token).min(right.units.len() - second.token);
    let mut best = None;
    for length in 1..=max_length {
        let left_end = first.token + length;
        let right_end = second.token + length;
        let Some(generalized) = generalize_units(
            &left.units[first.token..left_end],
            &right.units[second.token..right_end],
            options.max_holes,
        ) else {
            break;
        };
        if generalized.anchor_tokens < options.min_tokens {
            continue;
        }

        let left_range = unit_range(left, first.token, left_end);
        let right_range = unit_range(right, second.token, right_end);
        if left.file_id == right.file_id && ranges_overlap(left_range, right_range) {
            continue;
        }
        if physical_lines(&files[left.file_id], left_range) < options.min_lines
            || physical_lines(&files[right.file_id], right_range) < options.min_lines
        {
            continue;
        }

        best = Some(VerifiedMatch {
            canonical: generalized.canonical,
            tokens: generalized.anchor_tokens,
            left: (left.file_id, left_range.0, left_range.1),
            right: (right.file_id, right_range.0, right_range.1),
            left_holes: generalized.left_holes,
            right_holes: generalized.right_holes,
        });
    }
    best
}

fn insert_verified(raw_classes: &mut HashMap<Vec<CanonicalPart>, RawClass>, found: VerifiedMatch) {
    let holes = found.left_holes.len();
    let class = raw_classes
        .entry(found.canonical.clone())
        .or_insert_with(|| RawClass {
            canonical: found.canonical,
            tokens: found.tokens,
            holes,
            occurrences: BTreeSet::new(),
            hole_ranges: HashMap::new(),
        });
    class.tokens = class.tokens.min(found.tokens);
    class.occurrences.insert(found.left);
    class.occurrences.insert(found.right);
    class
        .hole_ranges
        .entry(found.left)
        .or_insert(found.left_holes);
    class
        .hole_ranges
        .entry(found.right)
        .or_insert(found.right_holes);
}

fn finish_report(
    raw_classes: HashMap<Vec<CanonicalPart>, RawClass>,
    files: &[ParsedFile],
    containers: &[Container],
    options: &Options,
) -> Report {
    let mut classes: Vec<_> = raw_classes
        .into_values()
        .filter(|class| class.occurrences.len() >= 2)
        .collect();
    classes.sort_by(|left, right| {
        right
            .tokens
            .cmp(&left.tokens)
            .then_with(|| left.holes.cmp(&right.holes))
            .then_with(|| left.occurrences.cmp(&right.occurrences))
    });
    classes = suppress_nested(classes);

    let mut clones: Vec<_> = classes
        .into_iter()
        .map(|class| materialize_class(class, files, containers))
        .collect();
    sort_clones(&mut clones, options.sort);

    let duplicated_lines = duplicated_lines(&clones);
    Report {
        schema_version: 2,
        summary: Summary {
            files: files.len(),
            clone_classes: clones.len(),
            duplicated_lines,
        },
        clones,
    }
}

fn sort_clones(clones: &mut [CloneClass], sort: SortOrder) {
    clones.sort_by(|left, right| {
        let ordering = match sort {
            SortOrder::Occurrences => compare_occurrences(left, right)
                .then_with(|| compare_impact(left, right))
                .then_with(|| compare_size(left, right)),
            SortOrder::Impact => compare_impact(left, right)
                .then_with(|| compare_occurrences(left, right))
                .then_with(|| compare_size(left, right)),
            SortOrder::Size => compare_size(left, right)
                .then_with(|| compare_occurrences(left, right))
                .then_with(|| compare_impact(left, right)),
            SortOrder::Spread => compare_spread(left, right)
                .then_with(|| compare_occurrences(left, right))
                .then_with(|| compare_impact(left, right)),
            SortOrder::Path => compare_first_location(left, right),
        };
        ordering
            .then_with(|| left.holes.cmp(&right.holes))
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn compare_occurrences(left: &CloneClass, right: &CloneClass) -> Ordering {
    right.occurrences.len().cmp(&left.occurrences.len())
}

fn compare_impact(left: &CloneClass, right: &CloneClass) -> Ordering {
    let left_impact = left.tokens * left.occurrences.len().saturating_sub(1);
    let right_impact = right.tokens * right.occurrences.len().saturating_sub(1);
    right_impact.cmp(&left_impact)
}

fn compare_size(left: &CloneClass, right: &CloneClass) -> Ordering {
    right
        .tokens
        .cmp(&left.tokens)
        .then_with(|| right.lines.cmp(&left.lines))
}

fn compare_spread(left: &CloneClass, right: &CloneClass) -> Ordering {
    let left_files = left
        .occurrences
        .iter()
        .map(|occurrence| &occurrence.path)
        .collect::<HashSet<_>>()
        .len();
    let right_files = right
        .occurrences
        .iter()
        .map(|occurrence| &occurrence.path)
        .collect::<HashSet<_>>()
        .len();
    right_files.cmp(&left_files)
}

fn compare_first_location(left: &CloneClass, right: &CloneClass) -> Ordering {
    match (first_occurrence(left), first_occurrence(right)) {
        (Some(left), Some(right)) => compare_occurrence_location(left, right),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn first_occurrence(class: &CloneClass) -> Option<&Occurrence> {
    class
        .occurrences
        .iter()
        .min_by(|left, right| compare_occurrence_location(left, right))
}

fn compare_occurrence_location(left: &Occurrence, right: &Occurrence) -> Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.start.line.cmp(&right.start.line))
        .then_with(|| left.start.column.cmp(&right.start.column))
}

fn verify_pair(
    first: Posting,
    second: Posting,
    containers: &[Container],
    files: &[ParsedFile],
    options: &Options,
) -> Vec<VerifiedMatch> {
    let left = &containers[first.container];
    let right = &containers[second.container];
    let k = options.min_tokens;
    let Some(left_seed) = left.rough.get(first.token..first.token + k) else {
        return Vec::new();
    };
    let Some(right_seed) = right.rough.get(second.token..second.token + k) else {
        return Vec::new();
    };
    if left_seed != right_seed {
        return Vec::new();
    }

    let mut left_start = first.token;
    let mut right_start = second.token;
    while left_start > 0
        && right_start > 0
        && left.rough[left_start - 1] == right.rough[right_start - 1]
    {
        left_start -= 1;
        right_start -= 1;
    }

    let mut left_end = first.token + k;
    let mut right_end = second.token + k;
    while left_end < left.rough.len()
        && right_end < right.rough.len()
        && left.rough[left_end] == right.rough[right_end]
    {
        left_end += 1;
        right_end += 1;
    }

    let Some(left_units) = complete_units(left, left_start, left_end) else {
        return Vec::new();
    };
    let Some(right_units) = complete_units(right, right_start, right_end) else {
        return Vec::new();
    };
    let candidates =
        qualifying_equal_subranges(left, left_units, right, right_units, files, options);
    let mut verified = Vec::new();

    for (mut left_unit_start, mut left_unit_end, mut right_unit_start, mut right_unit_end) in
        candidates
    {
        loop {
            let mut changed = false;
            if left_unit_start > 0
                && right_unit_start > 0
                && left.units[left_unit_start - 1].rough == right.units[right_unit_start - 1].rough
                && sequences_equal(
                    left,
                    left_unit_start - 1,
                    left_unit_end,
                    right,
                    right_unit_start - 1,
                    right_unit_end,
                )
            {
                left_unit_start -= 1;
                right_unit_start -= 1;
                changed = true;
            }
            if left_unit_end < left.units.len()
                && right_unit_end < right.units.len()
                && left.units[left_unit_end].rough == right.units[right_unit_end].rough
                && sequences_equal(
                    left,
                    left_unit_start,
                    left_unit_end + 1,
                    right,
                    right_unit_start,
                    right_unit_end + 1,
                )
            {
                left_unit_end += 1;
                right_unit_end += 1;
                changed = true;
            }
            if !changed {
                break;
            }
        }

        let left_range = unit_range(left, left_unit_start, left_unit_end);
        let right_range = unit_range(right, right_unit_start, right_unit_end);
        if left.file_id == right.file_id && ranges_overlap(left_range, right_range) {
            continue;
        }

        let tokens = left.units[left_unit_start..left_unit_end]
            .iter()
            .map(|unit| unit.tokens)
            .sum();
        if tokens < options.min_tokens
            || physical_lines(&files[left.file_id], left_range) < options.min_lines
            || physical_lines(&files[right.file_id], right_range) < options.min_lines
        {
            continue;
        }

        let canonical = canonical_structure(&left.units[left_unit_start..left_unit_end]);
        let found = VerifiedMatch {
            canonical,
            tokens,
            left: (left.file_id, left_range.0, left_range.1),
            right: (right.file_id, right_range.0, right_range.1),
            left_holes: Vec::new(),
            right_holes: Vec::new(),
        };
        if !verified.iter().any(|existing: &VerifiedMatch| {
            existing.left == found.left && existing.right == found.right
        }) {
            verified.push(found);
        }
    }
    verified
}

fn complete_units(container: &Container, start: usize, end: usize) -> Option<(usize, usize)> {
    let mut matches = container
        .unit_token_ranges
        .iter()
        .enumerate()
        .filter(|(_, (unit_start, unit_end))| *unit_start >= start && *unit_end <= end)
        .map(|(index, _)| index);
    let first = matches.next()?;
    let last = matches.next_back().unwrap_or(first);
    Some((first, last + 1))
}

fn qualifying_equal_subranges(
    left: &Container,
    left_units: (usize, usize),
    right: &Container,
    right_units: (usize, usize),
    files: &[ParsedFile],
    options: &Options,
) -> Vec<(usize, usize, usize, usize)> {
    let (left_start, left_end) = left_units;
    let (right_start, right_end) = right_units;
    let max_length = (left_end - left_start).min(right_end - right_start);
    let mut candidates = Vec::new();

    for length in 1..=max_length {
        for left_offset in 0..=left_end - left_start - length {
            for right_offset in 0..=right_end - right_start - length {
                let candidate_left_start = left_start + left_offset;
                let candidate_right_start = right_start + right_offset;
                let candidate_left_end = candidate_left_start + length;
                let candidate_right_end = candidate_right_start + length;
                if !sequences_equal(
                    left,
                    candidate_left_start,
                    candidate_left_end,
                    right,
                    candidate_right_start,
                    candidate_right_end,
                ) {
                    continue;
                }

                let tokens: usize = left.units[candidate_left_start..candidate_left_end]
                    .iter()
                    .map(|unit| unit.tokens)
                    .sum();
                if tokens < options.min_tokens {
                    continue;
                }
                let left_range = unit_range(left, candidate_left_start, candidate_left_end);
                let right_range = unit_range(right, candidate_right_start, candidate_right_end);
                if left.file_id == right.file_id && ranges_overlap(left_range, right_range) {
                    continue;
                }
                if physical_lines(&files[left.file_id], left_range) < options.min_lines
                    || physical_lines(&files[right.file_id], right_range) < options.min_lines
                {
                    continue;
                }

                candidates.push((
                    tokens,
                    (
                        candidate_left_start,
                        candidate_left_end,
                        candidate_right_start,
                        candidate_right_end,
                    ),
                ));
            }
        }
    }

    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    let mut maximal = Vec::new();
    for (_, candidate) in candidates {
        let contained = maximal.iter().any(|larger: &(usize, usize, usize, usize)| {
            larger.0 <= candidate.0
                && larger.1 >= candidate.1
                && larger.2 <= candidate.2
                && larger.3 >= candidate.3
        });
        if !contained {
            maximal.push(candidate);
        }
    }
    maximal
}

fn sequences_equal(
    left: &Container,
    left_start: usize,
    left_end: usize,
    right: &Container,
    right_start: usize,
    right_end: usize,
) -> bool {
    canonical_structure(&left.units[left_start..left_end])
        == canonical_structure(&right.units[right_start..right_end])
}

fn unit_range(container: &Container, start: usize, end: usize) -> (usize, usize) {
    (
        container.units[start].range.0,
        container.units[end - 1].range.1,
    )
}

fn ranges_overlap(left: (usize, usize), right: (usize, usize)) -> bool {
    left.0 < right.1 && right.0 < left.1
}

fn physical_lines(file: &ParsedFile, range: (usize, usize)) -> usize {
    let start = line_index(&file.line_starts, range.0);
    let end = line_index(&file.line_starts, range.1.saturating_sub(1));
    end - start + 1
}

fn suppress_nested(classes: Vec<RawClass>) -> Vec<RawClass> {
    let mut kept: Vec<RawClass> = Vec::new();
    'class: for class in classes {
        for larger in &kept {
            if larger.tokens < class.tokens {
                continue;
            }
            let all_contained = class.occurrences.iter().all(|small| {
                larger.occurrences.iter().any(|big| {
                    small.0 == big.0
                        && big.1 <= small.1
                        && big.2 >= small.2
                        && (big.1 < small.1
                            || big.2 > small.2
                            || (larger.holes == 0 && class.holes == 0))
                })
            });
            if all_contained {
                continue 'class;
            }
        }
        kept.push(class);
    }
    kept
}

fn materialize_class(
    class: RawClass,
    files: &[ParsedFile],
    containers: &[Container],
) -> CloneClass {
    let id = digest(&class.canonical);
    let template_range = class.occurrences.first().copied();
    let template_holes = template_range
        .and_then(|range| class.hole_ranges.get(&range))
        .cloned()
        .unwrap_or_default();
    let occurrences: Vec<_> = class
        .occurrences
        .into_iter()
        .map(|(file_id, start, end)| occurrence(&files[file_id], file_id, start, end))
        .collect();
    let lines = occurrences
        .iter()
        .map(|occurrence| {
            physical_lines(
                &files[occurrence.file_id],
                (occurrence.byte_start, occurrence.byte_end),
            )
        })
        .max()
        .unwrap_or(0);
    let syntax_template = template_range.map_or_else(String::new, |(file_id, start, end)| {
        render_template(
            containers,
            &files[file_id],
            file_id,
            start,
            end,
            &template_holes,
        )
    });
    CloneClass {
        id,
        tokens: class.tokens,
        lines,
        holes: class.holes,
        occurrences,
        syntax_template,
    }
}

fn render_template(
    containers: &[Container],
    file: &ParsedFile,
    file_id: usize,
    start: usize,
    end: usize,
    holes: &[HoleRange],
) -> String {
    for container in containers
        .iter()
        .filter(|container| container.file_id == file_id)
    {
        let Some(unit_start) = container
            .units
            .iter()
            .position(|unit| unit.range.0 == start)
        else {
            continue;
        };
        let Some(relative_end) = container.units[unit_start..]
            .iter()
            .position(|unit| unit.range.1 == end)
        else {
            continue;
        };
        return render_syntax_template(
            &container.units[unit_start..=unit_start + relative_end],
            &file.source,
            holes,
        );
    }
    file.source[start..end].to_owned()
}

fn occurrence(file: &ParsedFile, file_id: usize, start: usize, end: usize) -> Occurrence {
    Occurrence {
        path: file.path.clone(),
        start: position(file, start),
        end: position(file, end),
        source: String::new(),
        file_id,
        byte_start: start,
        byte_end: end,
    }
}

fn position(file: &ParsedFile, byte: usize) -> Position {
    let line = line_index(&file.line_starts, byte);
    let line_start = file.line_starts[line];
    let column = file.source[line_start..byte.min(file.source.len())]
        .chars()
        .count()
        + 1;
    Position {
        line: line + 1,
        column,
    }
}

fn line_index(line_starts: &[usize], byte: usize) -> usize {
    line_starts
        .partition_point(|start| *start <= byte)
        .saturating_sub(1)
}

fn duplicated_lines(classes: &[CloneClass]) -> usize {
    let mut intervals: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for class in classes {
        for occurrence in class.occurrences.iter().skip(1) {
            intervals
                .entry(occurrence.file_id)
                .or_default()
                .push((occurrence.start.line, occurrence.end.line));
        }
    }

    intervals
        .into_values()
        .map(|mut ranges| {
            ranges.sort_unstable();
            let mut total = 0;
            let mut current: Option<(usize, usize)> = None;
            for range in ranges {
                match current {
                    Some((start, end)) if range.0 <= end + 1 => {
                        current = Some((start, end.max(range.1)));
                    }
                    Some((start, end)) => {
                        total += end - start + 1;
                        current = Some(range);
                    }
                    None => current = Some(range),
                }
            }
            if let Some((start, end)) = current {
                total += end - start + 1;
            }
            total
        })
        .sum()
}

fn fingerprints(tokens: &[String], width: usize) -> Vec<(u64, usize)> {
    if width == 0 || tokens.len() < width {
        return Vec::new();
    }

    const BASE: u64 = 1_000_003;
    let mut power = 1_u64;
    for _ in 1..width {
        power = power.wrapping_mul(BASE);
    }

    let values: Vec<_> = tokens
        .iter()
        .map(|token| stable_hash(token.as_bytes()))
        .collect();
    let mut hash = 0_u64;
    for value in &values[..width] {
        hash = hash.wrapping_mul(BASE).wrapping_add(*value);
    }

    let mut output = vec![(hash, 0)];
    for start in 1..=values.len() - width {
        hash = hash
            .wrapping_sub(values[start - 1].wrapping_mul(power))
            .wrapping_mul(BASE)
            .wrapping_add(values[start + width - 1]);
        output.push((hash, start));
    }
    output
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn digest(parts: &[CanonicalPart]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        match part {
            CanonicalPart::Enter(kind) => {
                hasher.update(&[0]);
                hasher.update(&kind.to_le_bytes());
            }
            CanonicalPart::Leave(kind) => {
                hasher.update(&[1]);
                hasher.update(&kind.to_le_bytes());
            }
            CanonicalPart::Token(kind, text) => {
                hasher.update(&[2]);
                hasher.update(&kind.to_le_bytes());
                hasher.update(text.as_bytes());
                hasher.update(&[0]);
            }
            CanonicalPart::Local(id) => {
                hasher.update(&[3]);
                hasher.update(&id.to_le_bytes());
            }
            CanonicalPart::Hole(id) => {
                hasher.update(&[4]);
                hasher.update(&id.to_le_bytes());
            }
        }
    }
    hasher.finalize().to_hex()[..12].to_owned()
}

fn empty_report(files: usize) -> Report {
    Report {
        schema_version: 2,
        summary: Summary {
            files,
            clone_classes: 0,
            duplicated_lines: 0,
        },
        clones: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{fingerprints, sort_clones};
    use crate::model::{CloneClass, Occurrence, Position, SortOrder};

    fn clone_class(id: &str, tokens: usize, paths: &[&str]) -> CloneClass {
        CloneClass {
            id: id.to_owned(),
            tokens,
            lines: 1,
            holes: 0,
            occurrences: paths
                .iter()
                .enumerate()
                .map(|(index, path)| Occurrence {
                    path: PathBuf::from(path),
                    start: Position {
                        line: index + 1,
                        column: 1,
                    },
                    end: Position {
                        line: index + 1,
                        column: 2,
                    },
                    source: String::new(),
                    file_id: index,
                    byte_start: index,
                    byte_end: index + 1,
                })
                .collect(),
            syntax_template: String::new(),
        }
    }

    #[test]
    fn sort_modes_prioritize_their_documented_metric() {
        let widespread = clone_class("widespread", 10, &["m.rs", "m.rs", "m.rs"]);
        let impactful = clone_class("impactful", 30, &["b.rs", "c.rs"]);
        let early = clone_class("early", 5, &["a.rs", "z.rs"]);
        let classes = vec![widespread, impactful, early];

        for (sort, expected) in [
            (SortOrder::Occurrences, "widespread"),
            (SortOrder::Impact, "impactful"),
            (SortOrder::Size, "impactful"),
            (SortOrder::Spread, "impactful"),
            (SortOrder::Path, "early"),
        ] {
            let mut sorted = classes.clone();
            sort_clones(&mut sorted, sort);
            assert_eq!(sorted[0].id, expected, "unexpected order for {sort:?}");
        }
    }

    #[test]
    fn rolling_fingerprints_are_stable() {
        let tokens = ["a", "b", "c", "d"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(fingerprints(&tokens, 2), fingerprints(&tokens, 2));
        assert_eq!(fingerprints(&tokens, 5), Vec::new());
    }
}
