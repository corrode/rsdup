mod detect;
mod discover;
mod model;
pub mod ollama;
pub mod semantic;
mod syntax;

use std::path::PathBuf;

use anyhow::Result;

pub use discover::DiscoverOptions;
pub use model::{
    CloneClass, DetectionMode, Occurrence, Options, Position, Profile, Report, SortOrder, Summary,
};

pub fn scan(
    paths: &[PathBuf],
    options: &Options,
    discover_options: &DiscoverOptions,
) -> Result<Report> {
    let paths = discover::discover(paths, discover_options)?;
    let sources = discover::read_sources(&paths)?;
    analyze_sources(sources, options)
}

/// Scan body-level similarity candidates without changing syntax clone detection.
pub fn scan_semantic(
    paths: &[PathBuf],
    options: &semantic::SemanticOptions,
    discover_options: &DiscoverOptions,
    embedder: &mut impl semantic::Embedder,
) -> Result<semantic::SemanticReport> {
    options.validate()?;
    let paths = discover::discover(paths, discover_options)?;
    semantic::analyze(discover::read_sources(&paths)?, options, embedder)
}

pub fn analyze_sources(sources: Vec<(PathBuf, String)>, options: &Options) -> Result<Report> {
    let (files, containers) =
        syntax::parse_sources(sources, options.profile, options.ignore_tests)?;
    Ok(detect::detect(&files, &containers, options))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{DetectionMode, Options, Profile, SortOrder, analyze_sources};

    fn analyze(left: &str, right: &str, profile: Profile) -> super::Report {
        analyze_with(
            left,
            right,
            Options {
                min_tokens: 8,
                min_lines: 1,
                profile,
                mode: DetectionMode::Exact,
                max_holes: 1,
                ignore_tests: false,
                sort: SortOrder::Occurrences,
            },
        )
    }

    fn analyze_with(left: &str, right: &str, options: Options) -> super::Report {
        analyze_sources(
            vec![
                (PathBuf::from("left.rs"), left.to_owned()),
                (PathBuf::from("right.rs"), right.to_owned()),
            ],
            &options,
        )
        .unwrap()
    }

    #[test]
    fn finds_statement_aligned_duplicates_with_renamed_locals() {
        let report = analyze(
            "fn left(value: i32) {\n let doubled = value * 2;\n observe(doubled);\n doubled + 1\n}",
            "fn right(input: i32) {\n let result = input * 7;\n observe(result);\n result + 9\n}",
            Profile::Balanced,
        );
        assert_eq!(report.summary.clone_classes, 1);
        assert_eq!(report.clones[0].occurrences.len(), 2);
        assert!(
            report.clones[0]
                .syntax_template
                .contains("let $1 = $2 * $3;")
        );
        assert!(report.clones[0].syntax_template.contains("observe($1);"));
        assert!(report.clones[0].syntax_template.contains("$1 + $4"));
    }

    #[test]
    fn syntax_templates_do_not_change_the_json_schema() {
        let report = analyze(
            "fn left(value: i32) { let result = value + 1; consume(result); }",
            "fn right(input: i32) { let output = input + 2; consume(output); }",
            Profile::Balanced,
        );
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(report.schema_version, 2);
        assert!(json.contains("\"holes\":0"));
        assert!(!json.contains("syntax_template"));
        assert!(!json.contains("let $1"));
    }

    #[test]
    fn changed_method_names_do_not_match() {
        let report = analyze(
            "fn left(client: Client) {\n let item = client.load();\n client.send(item);\n item.finish();\n}",
            "fn right(cache: Cache) {\n let item = cache.fetch();\n cache.store(item);\n item.complete();\n}",
            Profile::Balanced,
        );
        assert!(report.clones.is_empty());
    }

    #[test]
    fn comments_and_formatting_do_not_affect_strict_matches() {
        let report = analyze(
            "fn left(value: i32) {\n let result = value + 1;\n consume(result);\n}",
            "fn right(value: i32) { let result=value+1; /* note */ consume(result); }",
            Profile::Strict,
        );
        assert_eq!(report.summary.clone_classes, 1);
    }

    #[test]
    fn strict_mode_preserves_names_and_values() {
        let report = analyze(
            "fn left(value: i32) {\n let result = value + 1;\n consume(result);\n}",
            "fn right(input: i32) {\n let output = input + 2;\n consume(output);\n}",
            Profile::Strict,
        );
        assert!(report.clones.is_empty());
    }

    #[test]
    fn later_nested_bindings_do_not_rename_out_of_scope_calls() {
        let report = analyze(
            "fn left() {\n helper();\n { let helper = 1; consume_left(helper); }\n}",
            "fn right() {\n service();\n { let service = 1; consume_right(service); }\n}",
            Profile::Balanced,
        );
        assert!(report.clones.is_empty());
    }

    #[test]
    fn closure_parameters_do_not_escape_the_closure() {
        let report = analyze_with(
            "fn x() {} fn left() { apply(|x| x); x(); done(); }",
            "fn y() {} fn right() { apply(|y| y); y(); done(); }",
            Options {
                min_tokens: 16,
                min_lines: 1,
                ..Options::default()
            },
        );
        assert!(report.clones.is_empty());
    }

    #[test]
    fn macro_contents_remain_semantic_anchors() {
        let report = analyze(
            "fn left() { query!(\"SELECT\", 10, true); left_done(); }",
            "fn right() { query!(\"DELETE\", 20, true); right_done(); }",
            Profile::Balanced,
        );
        assert!(report.clones.is_empty());
    }

    #[test]
    fn attribute_contents_remain_semantic_anchors() {
        let report = analyze_with(
            "fn left() { #[cfg(feature = \"left\")] work(); done(); }",
            "fn right() { #[cfg(feature = \"right\")] work(); done(); }",
            Options {
                min_tokens: 12,
                min_lines: 1,
                ..Options::default()
            },
        );
        assert!(report.clones.is_empty());
    }

    #[test]
    fn canonical_subclones_survive_rough_overextension() {
        let report = analyze_with(
            "fn left() { let a = 0; let b = 0; use_it(a); }",
            "fn right() { let x = 0; let y = 0; use_it(y); }",
            Options {
                min_tokens: 10,
                min_lines: 1,
                mode: DetectionMode::Exact,
                ..Options::default()
            },
        );
        assert_eq!(report.summary.clone_classes, 1);
        assert_eq!(report.clones[0].tokens, 10);
    }

    #[test]
    fn near_miss_is_the_default_detection_mode() {
        assert_eq!(Options::default().mode, DetectionMode::NearMiss);
    }

    #[test]
    fn near_miss_replaces_a_nested_pure_expression_with_a_hole() {
        let report = analyze_with(
            "fn left(policy: Policy, offset: u32) { let value = load(); validate(value, policy.level + offset); store(value); }",
            "fn right(flags: Flags, bits: u32) { let item = load(); validate(item, flags.mask & bits); store(item); }",
            Options {
                min_tokens: 12,
                min_lines: 1,
                ..Options::default()
            },
        );

        assert_eq!(report.summary.clone_classes, 1);
        assert_eq!(report.clones[0].holes, 1);
        assert!(
            report.clones[0]
                .syntax_template
                .contains("validate($1, $2);")
        );
    }

    #[test]
    fn exact_mode_rejects_the_same_near_miss() {
        let report = analyze_with(
            "fn left(policy: Policy, offset: u32) { let value = load(); validate(value, policy.level + offset); store(value); }",
            "fn right(flags: Flags, bits: u32) { let item = load(); validate(item, flags.mask & bits); store(item); }",
            Options {
                min_tokens: 12,
                min_lines: 1,
                mode: DetectionMode::Exact,
                ..Options::default()
            },
        );

        assert!(report.clones.is_empty());
    }

    #[test]
    fn near_miss_does_not_widen_two_sites_into_one_hole() {
        let sources = (
            "fn left(a: u32, b: u32, c: u32, d: u32, value: u32) { check(a + b, c * d); finish(value); }",
            "fn right(a: u32, b: u32, c: u32, d: u32, value: u32) { check(a - b, c / d); finish(value); }",
        );
        let one_hole = analyze_with(
            sources.0,
            sources.1,
            Options {
                min_tokens: 8,
                min_lines: 1,
                max_holes: 1,
                ..Options::default()
            },
        );
        let two_holes = analyze_with(
            sources.0,
            sources.1,
            Options {
                min_tokens: 8,
                min_lines: 1,
                max_holes: 2,
                ..Options::default()
            },
        );

        assert!(one_hole.clones.is_empty());
        assert_eq!(two_holes.summary.clone_classes, 1);
        assert_eq!(two_holes.clones[0].holes, 2);
    }

    #[test]
    fn near_miss_preserves_top_level_callees_and_nested_effects() {
        for (left, right) in [
            (
                "fn left(a: u32, b: u32) { prepare(); alpha(a + b); finish(); }",
                "fn right(a: u32, b: u32) { prepare(); beta(a - b); finish(); }",
            ),
            (
                "fn left() { prepare(); consume(load_one()); finish(); }",
                "fn right() { prepare(); consume(delete_all()); finish(); }",
            ),
        ] {
            let report = analyze_with(
                left,
                right,
                Options {
                    min_tokens: 8,
                    min_lines: 1,
                    ..Options::default()
                },
            );
            assert!(report.clones.is_empty());
        }
    }

    #[test]
    fn tokens_hidden_by_a_hole_do_not_satisfy_the_threshold() {
        let report = analyze_with(
            "fn left(a: u32, b: u32, c: u32, d: u32) { consume((a + b) * (c + d)); finish(); }",
            "fn right(a: u32, b: u32, c: u32, d: u32) { consume((a - b) / (c - d)); finish(); }",
            Options {
                min_tokens: 12,
                min_lines: 1,
                ..Options::default()
            },
        );

        assert!(report.clones.is_empty());
    }

    #[test]
    fn subclone_search_skips_undersized_earlier_matches() {
        let report = analyze_with(
            "fn left() { ping(); apply(|a, b| pair(a, b)); large(alpha(), beta(), gamma()); }",
            "fn right() { ping(); apply(|x, y| pair(x, x)); large(alpha(), beta(), gamma()); }",
            Options {
                min_tokens: 10,
                min_lines: 1,
                mode: DetectionMode::Exact,
                ..Options::default()
            },
        );
        assert!(report.clones.iter().any(|class| class.tokens >= 10));
    }

    #[test]
    fn all_disjoint_subclones_in_a_rough_run_are_reported() {
        let report = analyze_with(
            "fn left() { large(alpha(), beta(), gamma()); apply(|a, b| pair(a, b)); finish(one(), two()); }",
            "fn right() { large(alpha(), beta(), gamma()); apply(|x, y| pair(x, x)); finish(one(), two()); }",
            Options {
                min_tokens: 8,
                min_lines: 1,
                mode: DetectionMode::Exact,
                ..Options::default()
            },
        );
        assert_eq!(report.summary.clone_classes, 2);
    }

    #[test]
    fn clone_classes_are_sorted_by_occurrence_count() {
        let frequent = "fn duplicate() { let value = alpha(); alpha_use(value); }";
        let large = "fn duplicate() { let value = beta(); beta_one(value); beta_two(value); beta_three(value); }";
        let report = analyze_sources(
            vec![
                (PathBuf::from("a.rs"), frequent.to_owned()),
                (PathBuf::from("b.rs"), frequent.to_owned()),
                (PathBuf::from("c.rs"), frequent.to_owned()),
                (PathBuf::from("d.rs"), large.to_owned()),
                (PathBuf::from("e.rs"), large.to_owned()),
            ],
            &Options {
                min_tokens: 8,
                min_lines: 1,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(report.summary.clone_classes, 2);
        assert_eq!(report.clones[0].occurrences.len(), 3);
        assert_eq!(report.clones[1].occurrences.len(), 2);
        assert!(report.clones[1].tokens > report.clones[0].tokens);
    }

    #[test]
    fn frequent_fingerprints_are_not_discarded() {
        let body = "foo();".repeat(12);
        let report = analyze_with(
            &format!("fn left() {{ {body} }}"),
            &format!("fn right() {{ {body} }}"),
            Options {
                min_tokens: 4,
                min_lines: 1,
                mode: DetectionMode::Exact,
                ..Options::default()
            },
        );
        assert!(report.clones.iter().any(|class| {
            class
                .occurrences
                .iter()
                .any(|occurrence| occurrence.path == Path::new("left.rs"))
                && class
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence.path == Path::new("right.rs"))
        }));
    }

    #[test]
    fn fully_contained_repetitions_are_suppressed() {
        let report = analyze_with(
            "fn left() { foo(); foo(); bar(); }",
            "fn right() { foo(); foo(); bar(); }",
            Options {
                min_tokens: 4,
                min_lines: 1,
                mode: DetectionMode::Exact,
                ..Options::default()
            },
        );
        assert_eq!(report.summary.clone_classes, 1);
        assert_eq!(report.clones[0].tokens, 12);
    }

    #[test]
    fn pre_2024_identifiers_are_accepted() {
        let result = analyze_sources(
            vec![(PathBuf::from("edition_2021.rs"), "fn gen() {}".to_owned())],
            &Options::default(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn ignore_tests_excludes_test_functions_but_keeps_production_code() {
        let source = |production_name: &str, test_name: &str| {
            format!(
                "fn {production_name}() {{ let value = load(); process(value); finish(value); }}\n\
                 #[test] fn {test_name}() {{ let value = fixture(); process(value); finish(value); }}"
            )
        };
        let report = analyze_sources(
            vec![
                (PathBuf::from("left.rs"), source("left", "left_test")),
                (PathBuf::from("right.rs"), source("right", "right_test")),
            ],
            &Options {
                min_tokens: 8,
                min_lines: 1,
                ignore_tests: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(report.summary.clone_classes, 1);
        assert_eq!(report.clones[0].occurrences.len(), 2);
    }

    #[test]
    fn tests_are_excluded_by_default_and_can_be_included() {
        let source = "#[cfg(test)] mod tests { fn duplicate() { let value = fixture(); process(value); finish(value); } }";
        let report = analyze_sources(
            vec![
                (PathBuf::from("left.rs"), source.to_owned()),
                (PathBuf::from("right.rs"), source.to_owned()),
            ],
            &Options {
                min_tokens: 8,
                min_lines: 1,
                ignore_tests: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(report.clones.is_empty());
        assert!(Options::default().ignore_tests);
        let included = analyze(source, source, Profile::Strict);
        assert!(!included.clones.is_empty());
    }

    #[test]
    fn ignore_tests_excludes_test_only_statements() {
        let report = analyze_sources(
            vec![
                (
                    PathBuf::from("left.rs"),
                    "fn left() { #[cfg(test)] test_only(alpha(), beta(), gamma()); left_done(); }"
                        .to_owned(),
                ),
                (
                    PathBuf::from("right.rs"),
                    "fn right() { #[cfg(test)] test_only(alpha(), beta(), gamma()); right_done(); }"
                        .to_owned(),
                ),
            ],
            &Options {
                min_tokens: 8,
                min_lines: 1,
                ignore_tests: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(report.clones.is_empty());
    }

    #[test]
    fn ignore_tests_excludes_integration_test_files() {
        let source = "fn duplicate() { let value = fixture(); process(value); finish(value); }";
        let report = analyze_sources(
            vec![
                (PathBuf::from("tests/left.rs"), source.to_owned()),
                (PathBuf::from("tests/right.rs"), source.to_owned()),
            ],
            &Options {
                min_tokens: 8,
                min_lines: 1,
                ignore_tests: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(report.summary.files, 0);
        assert!(report.clones.is_empty());
    }

    #[test]
    fn nested_test_directories_are_excluded_in_both_syntax_modes() {
        for mode in [DetectionMode::Exact, DetectionMode::NearMiss] {
            for directory in ["src/adapters/test", "src/adapters/tests"] {
                let source = "fn a() { let value = fixture(); process(value); finish(value); }";
                let sources = vec![
                    (
                        PathBuf::from(format!("{directory}/left.rs")),
                        source.to_owned(),
                    ),
                    (
                        PathBuf::from(format!("{directory}/right.rs")),
                        source.to_owned(),
                    ),
                ];
                let mut options = Options {
                    min_tokens: 8,
                    min_lines: 1,
                    mode,
                    ..Default::default()
                };
                let excluded = analyze_sources(sources.clone(), &options).unwrap();
                assert_eq!(excluded.summary.files, 0);
                assert!(excluded.clones.is_empty());
                options.ignore_tests = false;
                assert!(
                    !analyze_sources(sources, &options)
                        .unwrap()
                        .clones
                        .is_empty()
                );
            }
        }
        for path in [
            "src/test_utils/input.rs",
            "src/contest/input.rs",
            "src/latest.rs",
        ] {
            assert!(!super::syntax::is_test_file(Path::new(path)));
        }
    }

    #[test]
    fn declarations_are_not_reported_as_executable_clones() {
        let report = analyze(
            "struct Left { alpha: i32, beta: String, gamma: bool }",
            "struct Right { alpha: i32, beta: String, gamma: bool }",
            Profile::Balanced,
        );
        assert!(report.clones.is_empty());
    }

    #[test]
    fn invalid_rust_is_an_operational_error() {
        let result = analyze_sources(
            vec![(PathBuf::from("broken.rs"), "fn broken( {".to_owned())],
            &Options::default(),
        );
        assert!(result.is_err());
    }
}
