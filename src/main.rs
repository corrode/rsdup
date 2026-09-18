mod clipboard;
mod config;
mod editor;
mod embedding_backend;
mod snippet_diff;
mod theme;
mod tui;
mod tui_data;

use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use bat::PrettyPrinter;
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use ra_ap_syntax::{
    AstNode, Edition, SourceFile,
    ast::{self, HasName},
};
use rsdup::{
    DetectionMode, DiscoverOptions, Options, Profile, Report, SortOrder, scan,
    semantic::{SemanticReport, SemanticUnit},
};

#[derive(Debug, Parser)]
#[command(
    name = "rsdup",
    version,
    about = "Find similar and duplicated Rust code",
    after_help = "Choosing a mode:\n  semantic (default)  Broad similarity review; uses FastEmbed or local Ollama embeddings.\n  near-miss           Offline, repeatable syntax checks; good for CI.\n  exact               Tightest syntax matches; add --profile strict to keep names/literals.\n\nStart with: rsdup .\nNo server? rsdup . --embedding-backend fastembed (~642 MB first download)\nNo model?  rsdup . --mode near-miss\nTests are excluded by default."
)]
struct Cli {
    /// Files or directories to scan. Defaults to the current directory.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Minimum significant Rust tokens in a syntax clone or semantic body.
    #[arg(short = 'k', long, default_value_t = 40, value_parser = positive_usize)]
    min_tokens: usize,

    /// Minimum number of physical source lines in each occurrence.
    #[arg(short = 'l', long, default_value_t = 5, value_parser = positive_usize)]
    min_lines: usize,

    /// Syntax modes only: normalize locals/literals (balanced) or preserve them (strict).
    #[arg(long, value_enum, default_value_t = ProfileArg::Balanced, conflicts_with = "embedding_model")]
    profile: ProfileArg,

    /// Detection strategy: verified syntax clones or embedding similarity candidates.
    #[arg(long, value_enum, default_value_t = ModeArg::Semantic)]
    mode: ModeArg,

    /// Maximum syntax holes in a near-miss clone.
    #[arg(long, default_value_t = 1, value_parser = positive_usize, conflicts_with = "embedding_model")]
    max_holes: usize,

    /// Semantic mode backend; auto uses Ollama if available, otherwise asks (not saved).
    #[arg(long, value_enum, default_value_t = embedding_backend::Backend::Auto)]
    embedding_backend: embedding_backend::Backend,

    /// Select an installed Ollama model (implies Ollama; conflicts with fastembed).
    #[arg(long)]
    embedding_model: Option<String>,

    /// Minimum raw cosine similarity in semantic mode (default: 0.93).
    #[arg(long, allow_hyphen_values = true)]
    similarity_threshold: Option<f64>,

    /// Opt-in persistent embedding cache directory (contains no source text).
    #[arg(long)]
    embedding_cache: Option<PathBuf>,

    /// Maximum comment-free body characters in semantic mode (default: 6000).
    #[arg(long, value_parser = positive_usize)]
    max_body_chars: Option<usize>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Presentation: summary, highlighted syntax, or an interactive source browser.
    #[arg(long, value_enum, default_value_t = View::Summary)]
    view: View,

    /// Editor command for the browser (then config, VISUAL, EDITOR, or zed).
    #[arg(long, value_name = "COMMAND")]
    editor: Option<String>,

    /// TUI color theme (overrides saved theme). See --list-themes; t opens the picker.
    #[arg(long, value_name = "NAME")]
    theme: Option<theme::ThemeName>,

    /// List available TUI themes without scanning or loading an embedding model.
    #[arg(long, exclusive = true)]
    list_themes: bool,

    /// Print the user config path without scanning or creating the file.
    #[arg(long, exclusive = true)]
    config_path: bool,

    /// Syntax modes only: how duplicate classes are ordered.
    #[arg(long, value_enum, default_value_t = SortArg::Occurrences, conflicts_with = "embedding_model")]
    sort: SortArg,

    /// Exclude paths matching this simple glob. May be repeated.
    #[arg(long, value_name = "GLOB", action = clap::ArgAction::Append)]
    exclude: Vec<String>,

    /// Do not honor .gitignore, .ignore, or global Git excludes.
    #[arg(long)]
    no_ignore: bool,

    /// Include integration tests, test files, and inline test-only code (excluded by default).
    #[arg(long)]
    include_tests: bool,

    /// Tests are already excluded by default; retained for compatibility.
    #[arg(long, hide = true, conflicts_with = "include_tests")]
    ignore_tests: bool,

    /// Exit with status 1 when syntax clones or semantic candidate groups are found.
    #[arg(long)]
    fail_on_duplicates: bool,

    /// Suppress normal report output, backend notices, and interactive prompts.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum ProfileArg {
    #[default]
    Balanced,
    Strict,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum ModeArg {
    /// Exact normalized syntax; use --profile strict to retain names and literals.
    Exact,
    /// Offline syntax clones with limited expression differences; useful for CI.
    NearMiss,
    /// Broader, model-dependent similarity candidates via FastEmbed or local Ollama.
    #[default]
    Semantic,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum Format {
    #[default]
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum View {
    #[default]
    Summary,
    Syntax,
    Interactive,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum SortArg {
    #[default]
    Occurrences,
    Impact,
    Size,
    Spread,
    Path,
}

fn positive_usize(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(number) if number > 0 => Ok(number),
        _ => Err("value must be a positive integer".to_owned()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) if is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
        || error
            .downcast_ref::<serde_json::Error>()
            .and_then(serde_json::Error::io_error_kind)
            == Some(io::ErrorKind::BrokenPipe)
}

fn run() -> Result<ExitCode> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    if cli.list_themes {
        print_themes(io::stdout().lock())?;
        return Ok(ExitCode::SUCCESS);
    }
    if cli.config_path {
        writeln!(io::stdout().lock(), "{}", config::path()?.display())?;
        return Ok(ExitCode::SUCCESS);
    }
    validate_mode_options(&cli, &matches)?;
    if cli.quiet && matches!(cli.format, Format::Json) {
        bail!("--quiet cannot be combined with --format json");
    }
    if cli.view != View::Summary && matches!(cli.format, Format::Json) {
        bail!("--view only applies to text output");
    }
    validate_interactive(&cli)?;
    if cli.view == View::Interactive && (!io::stdin().is_terminal() || !io::stdout().is_terminal())
    {
        bail!(
            "--view interactive needs an interactive terminal; use --view summary or --format json when redirecting output"
        );
    }

    if matches!(cli.mode, ModeArg::Semantic) {
        return run_semantic(&cli);
    }
    if cli.embedding_model.is_some()
        || cli.similarity_threshold.is_some()
        || cli.embedding_cache.is_some()
        || cli.max_body_chars.is_some()
    {
        bail!("embedding options require --mode semantic");
    }

    let options = Options {
        min_tokens: cli.min_tokens,
        min_lines: cli.min_lines,
        profile: match cli.profile {
            ProfileArg::Balanced => Profile::Balanced,
            ProfileArg::Strict => Profile::Strict,
        },
        mode: match cli.mode {
            ModeArg::Exact => DetectionMode::Exact,
            ModeArg::NearMiss => DetectionMode::NearMiss,
            ModeArg::Semantic => unreachable!("semantic mode is handled separately"),
        },
        max_holes: cli.max_holes,
        ignore_tests: !cli.include_tests,
        sort: match cli.sort {
            SortArg::Occurrences => SortOrder::Occurrences,
            SortArg::Impact => SortOrder::Impact,
            SortArg::Size => SortOrder::Size,
            SortArg::Spread => SortOrder::Spread,
            SortArg::Path => SortOrder::Path,
        },
    };
    let discover_options = DiscoverOptions {
        no_ignore: cli.no_ignore,
        excludes: cli.exclude,
    };
    let report = scan(&cli.paths, &options, &discover_options)?;

    if cli.view == View::Interactive {
        tui::run(tui_data::syntax(&report), cli.editor.as_deref(), cli.theme)?;
    } else if !cli.quiet {
        let stdout = io::stdout();
        let colored_output = stdout.is_terminal();
        let mut output = stdout.lock();
        match cli.format {
            Format::Text => match cli.view {
                View::Summary => print_text(&mut output, &report)?,
                View::Syntax => print_syntax(&mut output, &report, colored_output)?,
                View::Interactive => unreachable!("interactive view is handled separately"),
            },
            Format::Json => {
                serde_json::to_writer_pretty(&mut output, &report)?;
                writeln!(output)?;
            }
        }
    }

    if cli.fail_on_duplicates && !report.clones.is_empty() {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn print_themes(mut output: impl Write) -> io::Result<()> {
    for &theme in theme::ThemeName::all() {
        writeln!(
            output,
            "{:<20} {}{}",
            theme.slug(),
            theme.display_name(),
            if theme == theme::ThemeName::default() {
                " (default)"
            } else {
                ""
            }
        )?;
    }
    Ok(())
}

fn validate_interactive(cli: &Cli) -> Result<()> {
    if cli.view == View::Interactive && cli.quiet {
        bail!("--quiet cannot be combined with --view interactive");
    }
    if cli.editor.is_some() && cli.view != View::Interactive {
        bail!("--editor only applies to --view interactive");
    }
    if cli.theme.is_some() && cli.view != View::Interactive {
        bail!("--theme only applies to --view interactive");
    }
    Ok(())
}

fn validate_mode_options(cli: &Cli, matches: &clap::ArgMatches) -> Result<()> {
    cli.embedding_backend
        .with_model(cli.embedding_model.as_deref())?;
    if !matches!(cli.mode, ModeArg::Semantic) {
        for name in [
            "embedding_backend",
            "embedding_model",
            "similarity_threshold",
            "embedding_cache",
            "max_body_chars",
        ] {
            if matches.value_source(name) == Some(clap::parser::ValueSource::CommandLine) {
                bail!("--{} requires --mode semantic", name.replace('_', "-"));
            }
        }
    }
    if matches!(cli.mode, ModeArg::Semantic) {
        for name in ["profile", "max_holes", "sort"] {
            if matches.value_source(name) == Some(clap::parser::ValueSource::CommandLine) {
                bail!(
                    "--{} only applies to syntax modes; use --mode near-miss or --mode exact",
                    name.replace('_', "-")
                );
            }
        }
    }
    Ok(())
}

fn run_semantic(cli: &Cli) -> Result<ExitCode> {
    use rsdup::semantic::SemanticOptions;

    let defaults = SemanticOptions::default();
    let options = SemanticOptions {
        min_tokens: cli.min_tokens,
        min_lines: cli.min_lines,
        ignore_tests: !cli.include_tests,
        threshold: cli.similarity_threshold.unwrap_or(defaults.threshold),
        max_chars: cli.max_body_chars.unwrap_or(defaults.max_chars),
    };
    options.validate()?;
    let discovery = DiscoverOptions {
        no_ignore: cli.no_ignore,
        excludes: cli.exclude.clone(),
    };
    let mut embedder = embedding_backend::BackendEmbedder::new(
        cli.embedding_backend,
        cli.embedding_model.as_deref(),
        cli.embedding_cache.clone(),
        cli.quiet,
    )?;
    let report = rsdup::scan_semantic(&cli.paths, &options, &discovery, &mut embedder)?;
    embedder.verify_model()?;
    if cli.view == View::Interactive {
        tui::run(
            tui_data::semantic(&report),
            cli.editor.as_deref(),
            cli.theme,
        )?;
    } else if !cli.quiet {
        let stdout = io::stdout();
        let colored = stdout.is_terminal();
        let mut output = stdout.lock();
        match cli.format {
            Format::Json => {
                #[derive(serde::Serialize)]
                struct Output<'a> {
                    #[serde(flatten)]
                    report: &'a rsdup::semantic::SemanticReport,
                    embedding: embedding_backend::Metadata,
                }
                serde_json::to_writer_pretty(
                    &mut output,
                    &Output {
                        report: &report,
                        embedding: embedder.metadata(),
                    },
                )?;
                writeln!(output)?;
            }
            Format::Text => print_semantic(&mut output, &report, cli.view, colored)?,
        }
    }
    Ok(if cli.fail_on_duplicates && !report.clusters.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn counted(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn print_semantic(
    mut output: impl Write,
    report: &SemanticReport,
    view: View,
    colored: bool,
) -> Result<()> {
    let files = counted(report.summary.files, "Rust file", "Rust files");
    if report.clusters.is_empty() {
        writeln!(output, "No similar code found in {files}.")?;
    } else {
        writeln!(
            output,
            "Found {} of similar code in {files}.",
            counted(report.clusters.len(), "group", "groups")
        )?;
    }
    for (number, cluster) in report.clusters.iter().enumerate() {
        let count = cluster.members.len();
        let same_body = cluster.members.first().is_some_and(|&first| {
            cluster
                .members
                .iter()
                .all(|&index| report.units[index].body_hash == report.units[first].body_hash)
        });
        write!(
            output,
            "\n{}. {} · {count} locations",
            number + 1,
            if same_body {
                "Identical bodies (ignoring comments/formatting)"
            } else {
                "Similar code"
            }
        )?;
        if !same_body && let Some(first) = cluster.edges.first() {
            let (min, max) = cluster
                .edges
                .iter()
                .fold((first.cosine, first.cosine), |(min, max), edge| {
                    (min.min(edge.cosine), max.max(edge.cosine))
                });
            let min = format!("{min:.2}");
            let max = format!("{max:.2}");
            write!(output, " · cosine {min}")?;
            if min != max {
                write!(output, "–{max}")?;
            }
        }
        writeln!(output)?;
        let pairs = count * count.saturating_sub(1) / 2;
        if cluster.edges.len() < pairs {
            writeln!(
                output,
                "  {} of {pairs} pairs matched; not every pair is similar.",
                cluster.edges.len()
            )?;
        }
        for (local, &index) in cluster.members.iter().enumerate() {
            let unit = &report.units[index];
            write!(output, "  ")?;
            if view == View::Syntax {
                write!(output, "[{}] ", local + 1)?;
            }
            writeln!(
                output,
                "{}{}",
                semantic_name(unit),
                if unit.test_context { " [test]" } else { "" }
            )?;
            writeln!(output, "    {}:{}", unit.path.display(), unit.start.line)?;
            if view == View::Syntax {
                for line in highlight_rust(&unit.source, colored)?.lines() {
                    writeln!(output, "    {line}")?;
                }
            }
        }
        if view == View::Syntax {
            writeln!(output, "  Matched pairs (local location numbers):")?;
            for edge in &cluster.edges {
                let left = cluster
                    .members
                    .iter()
                    .position(|&index| index == edge.left)
                    .context("semantic edge has no left member")?
                    + 1;
                let right = cluster
                    .members
                    .iter()
                    .position(|&index| index == edge.right)
                    .context("semantic edge has no right member")?
                    + 1;
                writeln!(
                    output,
                    "    [{left}] ↔ [{right}]: cosine {:.4}{}",
                    edge.cosine,
                    if edge.exact_body {
                        " (same token body)"
                    } else {
                        ""
                    }
                )?;
            }
        }
    }
    let summary = &report.summary;
    let skipped: Vec<_> = [
        (summary.skipped_small, "small"),
        (summary.skipped_large, "large"),
        (summary.skipped_tests, "test-context"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, reason)| format!("{count} {reason}"))
    .collect();
    if !skipped.is_empty() {
        if !report.clusters.is_empty() {
            writeln!(output)?;
        }
        writeln!(output, "Skipped bodies: {}.", skipped.join(", "))?;
    }
    if summary.suppressed_overlaps > 0 {
        writeln!(
            output,
            "Suppressed {} overlapping group members; all eligible bodies remain in JSON.",
            summary.suppressed_overlaps
        )?;
    }
    if !report.clusters.is_empty() {
        writeln!(
            output,
            "\nSimilarity is not equivalence. Use --view syntax to compare bodies or --format json for full evidence."
        )?;
    }
    Ok(())
}

fn semantic_name(unit: &SemanticUnit) -> String {
    // Headers may be incomplete and contain attributes, comments, or multiline generics.
    let tree = SourceFile::parse(&unit.context, Edition::Edition2024).tree();
    let name = tree
        .syntax()
        .descendants()
        .find_map(ast::Fn::cast)
        .and_then(|function| function.name());
    let label = if let Some(name) = name {
        format!("{name}()")
    } else {
        let compact = unit
            .context
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut chars = compact.chars();
        let mut label: String = chars.by_ref().take(100).collect();
        if chars.next().is_some() {
            label.push('…');
        }
        label
    };
    match unit.kind.as_str() {
        "closure" => format!("{label} (closure)"),
        "async" => format!("{label} (async block)"),
        _ => label,
    }
}

fn print_syntax(mut output: impl Write, report: &Report, colored_output: bool) -> Result<()> {
    if report.clones.is_empty() {
        writeln!(
            output,
            "No duplicates found in {}.",
            counted(report.summary.files, "Rust file", "Rust files")
        )?;
        return Ok(());
    }

    for clone in &report.clones {
        writeln!(output, "{clone}")?;
        writeln!(output, "  syntax:")?;
        let highlighted = highlight_rust(&clone.syntax_template, colored_output)?;
        for line in highlighted.lines() {
            writeln!(output, "    {line}")?;
        }
        writeln!(output, "  occurrences:")?;
        for occurrence in &clone.occurrences {
            writeln!(
                output,
                "    {}:{}:{}-{}:{}",
                occurrence.path.display(),
                occurrence.start.line,
                occurrence.start.column,
                occurrence.end.line,
                occurrence.end.column
            )?;
        }
        writeln!(output)?;
    }

    writeln!(
        output,
        "Found {} across {} ({}).",
        counted(report.summary.clone_classes, "clone class", "clone classes"),
        counted(report.summary.files, "file", "files"),
        counted(
            report.summary.duplicated_lines,
            "duplicated line",
            "duplicated lines"
        )
    )?;
    Ok(())
}

fn highlight_rust(source: &str, colored_output: bool) -> Result<String> {
    let mut highlighted = String::new();
    PrettyPrinter::new()
        .input_from_bytes(source.as_bytes())
        .language("Rust")
        .line_numbers(true)
        .grid(true)
        .colored_output(colored_output)
        .true_color(colored_output)
        .print_with_writer(Some(&mut highlighted))
        .context("failed to syntax-highlight duplicate with bat")?;
    Ok(highlighted)
}

fn print_text(mut output: impl Write, report: &Report) -> io::Result<()> {
    if report.clones.is_empty() {
        writeln!(
            output,
            "No duplicates found in {}.",
            counted(report.summary.files, "Rust file", "Rust files")
        )?;
        return Ok(());
    }

    for (number, clone) in report.clones.iter().enumerate() {
        write!(
            output,
            "{}. Repeated code · {} · {} · {}",
            number + 1,
            counted(clone.occurrences.len(), "location", "locations"),
            counted(clone.lines, "line", "lines"),
            counted(clone.tokens, "anchored token", "anchored tokens")
        )?;
        if clone.holes > 0 {
            write!(output, " · {}", counted(clone.holes, "hole", "holes"))?;
        }
        writeln!(output)?;
        for occurrence in &clone.occurrences {
            writeln!(
                output,
                "  {}:{}",
                occurrence.path.display(),
                occurrence.start.line
            )?;
        }
        writeln!(output)?;
    }

    writeln!(
        output,
        "Found {} across {} ({}).",
        counted(report.summary.clone_classes, "clone class", "clone classes"),
        counted(report.summary.files, "file", "files"),
        counted(
            report.summary.duplicated_lines,
            "duplicated line",
            "duplicated lines"
        )
    )
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use rsdup::semantic::{self, Embedder, SemanticOptions, SemanticReport};

    use super::{Cli, ModeArg, View, highlight_rust, print_semantic, print_text, semantic_name};

    #[test]
    fn help_keeps_mode_guidance_without_trailing_disclaimer() {
        let help = <Cli as clap::CommandFactory>::command()
            .render_long_help()
            .to_string();
        assert!(help.contains("Choosing a mode:"));
        assert!(help.contains("Tests are excluded by default."));
        assert!(!help.contains("Similarity is not proof of equivalent behavior."));
    }

    struct SyntheticEmbedder;

    impl Embedder for SyntheticEmbedder {
        fn embed(&mut self, body: &str) -> anyhow::Result<Vec<f64>> {
            let angle: f64 = if body.contains("isolated") {
                std::f64::consts::PI
            } else if body.contains("middle") {
                0.3
            } else if body.contains("rounded_last") {
                0.6001
            } else if body.contains("last") {
                0.7
            } else {
                0.0
            };
            Ok(vec![angle.cos(), angle.sin()])
        }
    }

    fn semantic_report(source: &str) -> SemanticReport {
        semantic::analyze(
            vec![("project/src/example.rs".into(), source.into())],
            &SemanticOptions {
                min_tokens: 1,
                min_lines: 1,
                threshold: 0.9,
                ignore_tests: false,
                ..SemanticOptions::default()
            },
            &mut SyntheticEmbedder,
        )
        .unwrap()
    }

    fn render_semantic(report: &SemanticReport, view: View) -> String {
        let mut output = Vec::new();
        print_semantic(&mut output, report, view, false).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn semantic_summary_is_compact_and_uses_names_not_ids() {
        let report = semantic_report(
            "fn ignored() { isolated(); }\nfn first() { work(); }\n#[test]\nfn second() { work(); }",
        );
        let output = render_semantic(&report, View::Summary);
        assert_eq!(
            output,
            concat!(
                "Found 1 group of similar code in 1 Rust file.\n",
                "\n1. Identical bodies (ignoring comments/formatting) · 2 locations\n",
                "  first()\n    project/src/example.rs:2\n",
                "  second() [test]\n    project/src/example.rs:4\n",
                "\nSimilarity is not equivalence. Use --view syntax to compare bodies or --format json for full evidence.\n",
            )
        );
        for unit in &report.units {
            assert!(!output.contains(&unit.body_hash));
            assert!(!output.contains(&unit.id));
        }
        for cluster in &report.clusters {
            assert!(!output.contains(&cluster.id));
        }
    }

    #[test]
    fn semantic_lead_pluralizes_groups_and_files() {
        for count in [0, 1, 2] {
            let report = semantic::analyze(
                (0..count).map(|index| (
                    format!("source{index}.rs").into(),
                    "fn a() { work(); } fn b() { work(); } fn c() { isolated(); } fn d() { isolated(); }".into(),
                )).collect(),
                &SemanticOptions { min_tokens: 1, min_lines: 1, ..SemanticOptions::default() },
                &mut SyntheticEmbedder,
            ).unwrap();
            let output = render_semantic(&report, View::Summary);
            let expected = match count {
                0 => "No similar code found in 0 Rust files.",
                1 => "Found 2 groups of similar code in 1 Rust file.",
                _ => "Found 2 groups of similar code in 2 Rust files.",
            };
            assert_eq!(output.lines().next(), Some(expected));
        }
    }

    #[test]
    fn semantic_cosine_collapses_ranges_at_display_precision() {
        for source in [
            "fn a() { first(); } fn b() { middle(); }",
            "fn a() { first(); } fn b() { middle(); } fn c() { rounded_last(); }",
        ] {
            let report = semantic_report(source);
            let count = report.clusters[0].members.len();
            if count == 3 {
                let edges = &report.clusters[0].edges;
                assert_eq!(edges.len(), 2);
                assert_ne!(edges[0].cosine, edges[1].cosine);
            }
            let output = render_semantic(&report, View::Summary);
            assert_eq!(
                output.lines().nth(2).unwrap(),
                format!("1. Similar code · {count} locations · cosine 0.96")
            );
        }
    }

    #[test]
    fn identical_header_still_distinguishes_literal_values() {
        let report = semantic_report("fn a() { work(1); } fn b() { work(2); }");
        let output = render_semantic(&report, View::Summary);
        assert_eq!(
            output.lines().nth(2),
            Some("1. Similar code · 2 locations · cosine 1.00")
        );
        assert!(!output.contains("Identical bodies"));
    }

    #[test]
    fn skipped_footer_is_separated_from_nonempty_groups() {
        let report = semantic::analyze(
            vec![(
                "source.rs".into(),
                "fn small() {} fn a() { work(); } fn b() { work(); }".into(),
            )],
            &SemanticOptions {
                min_tokens: 3,
                min_lines: 1,
                ..SemanticOptions::default()
            },
            &mut SyntheticEmbedder,
        )
        .unwrap();
        for view in [View::Summary, View::Syntax] {
            let output = render_semantic(&report, view);
            assert!(output.contains("\n\nSkipped bodies: 1 small.\n"));
        }
    }

    #[test]
    fn syntax_singular_grammar_and_hole_counts() {
        let mut report = rsdup::analyze_sources(
            vec![(
                "source.rs".into(),
                "fn a() { work(); } fn b() { work(); }".into(),
            )],
            &rsdup::Options {
                min_tokens: 3,
                min_lines: 1,
                ..rsdup::Options::default()
            },
        )
        .unwrap();
        assert_eq!(report.clones.len(), 1);
        for holes in [0, 1, 2] {
            report.clones[0].holes = holes;
            let clone = &report.clones[0];
            let mut output = Vec::new();
            print_text(&mut output, &report).unwrap();
            let output = String::from_utf8(output).unwrap();
            let suffix = match holes {
                0 => "",
                1 => " · 1 hole",
                _ => " · 2 holes",
            };
            assert_eq!(
                output.lines().next().unwrap(),
                format!(
                    "1. Repeated code · 2 locations · 1 line · {} anchored tokens{suffix}",
                    clone.tokens
                )
            );
            assert_eq!(
                output.lines().last().unwrap(),
                "Found 1 clone class across 1 file (1 duplicated line)."
            );
        }
        let empty = rsdup::analyze_sources(
            vec![("source.rs".into(), String::new())],
            &rsdup::Options::default(),
        )
        .unwrap();
        let mut output = Vec::new();
        print_text(&mut output, &empty).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "No duplicates found in 1 Rust file.\n"
        );
        let mut output = Vec::new();
        super::print_syntax(&mut output, &empty, false).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "No duplicates found in 1 Rust file.\n"
        );
    }

    #[test]
    fn semantic_chain_uses_only_accepted_edges_for_range_and_warns() {
        let report = semantic_report(
            "fn ignored() { isolated(); }\nfn a() { first(); }\nfn b() { middle(); }\nfn c() { last(); }",
        );
        assert_eq!(report.clusters[0].members, [1, 2, 3]);
        assert_eq!(report.clusters[0].edges.len(), 2);
        let output = render_semantic(&report, View::Summary);
        assert!(output.contains("1. Similar code · 3 locations · cosine 0.92–0.96\n"));
        assert!(output.contains("2 of 3 pairs matched; not every pair is similar."));
        assert!(!output.contains('↔'));
        assert!(!output.contains("[1]"));
        assert!(!output.contains("0.76"));

        let detailed = render_semantic(&report, View::Syntax);
        assert!(detailed.contains("[1] a()"));
        assert!(detailed.contains("[2] b()"));
        assert!(detailed.contains("[3] c()"));
        assert!(detailed.contains("[1] ↔ [2]: cosine 0.9553"));
        assert!(detailed.contains("[2] ↔ [3]: cosine 0.9211"));
        assert_eq!(detailed.matches('↔').count(), 2);
        assert!(!detailed.contains("[1] ↔ [3]"));
        for unit in &report.units[1..] {
            assert!(detailed.contains(&unit.source));
        }
    }

    #[test]
    fn semantic_detailed_preserves_exact_edge_evidence() {
        let report =
            semantic_report("fn a() { /* original comment */ work(); }\nfn b() { work(); }");
        let output = render_semantic(&report, View::Syntax);
        assert!(output.contains("/* original comment */"));
        assert!(output.contains("[1] ↔ [2]: cosine 1.0000 (same token body)"));
        assert!(!output.contains("not every pair"));
        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn empty_semantic_report_has_no_empty_sections_or_zero_skip_counts() {
        for source in ["", "fn alone() { work(); }"] {
            let report = semantic_report(source);
            for view in [View::Summary, View::Syntax] {
                assert_eq!(
                    render_semantic(&report, view),
                    "No similar code found in 1 Rust file.\n"
                );
            }
        }
    }

    #[test]
    fn semantic_skip_counts_only_show_nonzero_reasons() {
        let report = semantic::analyze(
            vec![(
                "source.rs".into(),
                "fn small() {}\nfn large() { a(); b(); c(); }\n#[test] fn test() {}".into(),
            )],
            &SemanticOptions {
                min_tokens: 3,
                min_lines: 1,
                max_chars: 8,
                ignore_tests: true,
                ..SemanticOptions::default()
            },
            &mut SyntheticEmbedder,
        )
        .unwrap();
        let output = render_semantic(&report, View::Summary);
        assert_eq!(
            output,
            "No similar code found in 1 Rust file.\nSkipped bodies: 1 small, 1 large, 1 test-context.\n"
        );
        let small = semantic::analyze(
            vec![("source.rs".into(), "fn small() {}".into())],
            &SemanticOptions::default(),
            &mut SyntheticEmbedder,
        )
        .unwrap();
        assert_eq!(
            render_semantic(&small, View::Summary),
            "No similar code found in 1 Rust file.\nSkipped bodies: 1 small.\n"
        );
    }

    #[test]
    fn semantic_names_handle_multiline_unicode_comments_and_nested_bodies() {
        let report = semantic_report(concat!(
            "/// fn misleading()\n",
            "#[doc = \"fn also_wrong()\"]\n",
            "pub async fn café<T>(\n    value: T,\n) -> Result<(), T>\n",
            "where T: Fn() -> () {\n",
            "    let closure = || { work(); };\n",
            "    let future = async { work(); };\n",
            "    Ok(())\n}\n",
            "fn r#type() { work(); }\n",
        ));
        let names: Vec<_> = report.units.iter().map(semantic_name).collect();
        assert_eq!(
            names,
            [
                "café()",
                "café() (closure)",
                "café() (async block)",
                "r#type()"
            ]
        );
        let output = render_semantic(&report, View::Summary);
        assert!(!output.contains("misleading"));
        assert!(!output.contains("value: T"));
        assert!(output.contains("Suppressed "));
    }

    #[test]
    fn semantic_name_fallback_is_unicode_safe_and_single_line() {
        let source = format!(
            "const VALUE: () = {{ let callback = |{}| {{ work(); }}; }};",
            "é".repeat(120)
        );
        let report = semantic_report(&source);
        let name = semantic_name(&report.units[0]);
        assert_eq!(name, format!("|{}… (closure)", "é".repeat(99)));
        assert!(!name.contains('\n'));
    }

    #[test]
    fn syntax_summary_numbers_groups_and_keeps_full_paths() {
        let source = "fn example() {\n let value = fetch();\n consume(value);\n}";
        let report = rsdup::analyze_sources(
            vec![
                ("project/src/a.rs".into(), source.into()),
                ("project/src/b.rs".into(), source.into()),
            ],
            &rsdup::Options {
                min_tokens: 8,
                min_lines: 1,
                ..rsdup::Options::default()
            },
        )
        .unwrap();
        assert!(!report.clones.is_empty());
        let mut output = Vec::new();
        print_text(&mut output, &report).unwrap();
        let output = String::from_utf8(output).unwrap();
        for (number, clone) in report.clones.iter().enumerate() {
            assert!(output.contains(&format!(
                "{}. Repeated code · {} locations · {} lines · {} anchored tokens",
                number + 1,
                clone.occurrences.len(),
                clone.lines,
                clone.tokens
            )));
            assert!(!output.contains(&clone.id));
            for occurrence in &clone.occurrences {
                assert!(output.lines().any(|line| line
                    == format!("  {}:{}", occurrence.path.display(), occurrence.start.line)));
            }
        }
        assert!(!output.contains("0 holes"));
    }

    #[test]
    fn semantic_is_default_and_syntax_modes_remain_explicit() {
        use clap::{CommandFactory, FromArgMatches};
        for args in [vec!["rsdup"], vec!["rsdup", "."]] {
            let matches = Cli::command().try_get_matches_from(args).unwrap();
            let cli = Cli::from_arg_matches(&matches).unwrap();
            assert!(matches!(cli.mode, ModeArg::Semantic));
            assert!(matches!(ModeArg::default(), ModeArg::Semantic));
            assert!(cli.embedding_model.is_none());
            assert!(!cli.include_tests);
            super::validate_mode_options(&cli, &matches).unwrap();
        }
        let matches = Cli::command()
            .try_get_matches_from(["rsdup", "--profile", "strict"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        let error = super::validate_mode_options(&cli, &matches)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--mode near-miss or --mode exact"));
        for mode in ["near-miss", "exact"] {
            let matches = Cli::command()
                .try_get_matches_from(["rsdup", "--mode", mode, "--profile", "strict"])
                .unwrap();
            let cli = Cli::from_arg_matches(&matches).unwrap();
            assert!(!matches!(cli.mode, ModeArg::Semantic));
            super::validate_mode_options(&cli, &matches).unwrap();
        }
    }

    #[test]
    fn short_and_long_help_explain_mode_selection() {
        use clap::CommandFactory;
        for help in [
            Cli::command().render_help(),
            Cli::command().render_long_help(),
        ] {
            let help = help.to_string();
            assert!(help.contains("[default: semantic]"));
            assert!(help.contains("Choosing a mode:"));
            assert!(help.contains("uses FastEmbed or local Ollama embeddings"));
            assert!(help.contains("--embedding-backend"));
            assert!(help.contains("good for CI"));
            assert!(help.contains("--mode near-miss"));
            assert!(help.contains("--profile strict"));
        }
    }

    #[test]
    fn cli_themes_use_the_library_catalog() {
        for &name in crate::theme::ThemeName::all() {
            let cli =
                Cli::try_parse_from(["rsdup", "--view", "interactive", "--theme", name.slug()])
                    .unwrap();
            assert_eq!(cli.theme, Some(name));
            super::validate_interactive(&cli).unwrap();
        }
        let cli = Cli::try_parse_from(["rsdup", "--view", "interactive"]).unwrap();
        assert_eq!(
            cli.theme.unwrap_or_default(),
            crate::theme::ThemeName::Dracula
        );
        assert!(Cli::try_parse_from(["rsdup", "--theme", "nonexistent-theme"]).is_err());
        let cli = Cli::try_parse_from(["rsdup", "--theme", "nord"]).unwrap();
        assert!(
            super::validate_interactive(&cli)
                .unwrap_err()
                .to_string()
                .contains("--theme")
        );
        let cli = Cli::try_parse_from(["rsdup", "--list-themes"]).unwrap();
        assert!(cli.list_themes);
        assert!(Cli::try_parse_from(["rsdup", ".", "--list-themes"]).is_err());
        let mut output = Vec::new();
        super::print_themes(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.lines().count(), crate::theme::ThemeName::all().len());
        for &name in crate::theme::ThemeName::all() {
            assert!(output.contains(name.slug()));
        }
        assert!(output.contains("(default)"));
    }

    #[test]
    fn interactive_options_are_validated_before_scanning() {
        let cli = Cli::try_parse_from(["rsdup", "--view", "interactive", "--editor", "zed --wait"])
            .unwrap();
        super::validate_interactive(&cli).unwrap();
        assert_eq!(cli.editor.as_deref(), Some("zed --wait"));
        let quiet = Cli::try_parse_from(["rsdup", "--view", "interactive", "--quiet"]).unwrap();
        assert!(
            super::validate_interactive(&quiet)
                .unwrap_err()
                .to_string()
                .contains("--quiet")
        );
        let summary = Cli::try_parse_from(["rsdup", "--editor", "zed"]).unwrap();
        assert!(
            super::validate_interactive(&summary)
                .unwrap_err()
                .to_string()
                .contains("--editor")
        );
    }

    #[test]
    fn accepts_multiple_path_exclusions() {
        let cli = Cli::try_parse_from([
            "rsdup",
            "--exclude",
            "generated/**",
            "--exclude",
            "vendor/**",
            ".",
        ])
        .unwrap();

        assert_eq!(cli.exclude, ["generated/**", "vendor/**"]);
        assert!(matches!(cli.mode, ModeArg::Semantic));
        assert_eq!(cli.max_holes, 1);
    }

    #[test]
    fn semantic_auto_selection_accepts_defaults_but_not_explicit_syntax_controls() {
        use clap::{CommandFactory, FromArgMatches};
        let args = ["rsdup", ".", "--mode", "semantic"];
        let matches = Cli::command().try_get_matches_from(args).unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        assert!(cli.embedding_model.is_none());
        assert!(super::validate_mode_options(&cli, &matches).is_ok());
        for option in [
            ["--profile", "balanced"],
            ["--max-holes", "1"],
            ["--sort", "occurrences"],
        ] {
            let matches = Cli::command()
                .try_get_matches_from(args.into_iter().chain(option))
                .unwrap();
            let cli = Cli::from_arg_matches(&matches).unwrap();
            assert!(super::validate_mode_options(&cli, &matches).is_err());
        }
    }

    #[test]
    fn tests_are_opt_in_for_all_modes() {
        for mode in ["exact", "near-miss", "semantic"] {
            let args = ["rsdup", ".", "--mode", mode];
            assert!(!Cli::try_parse_from(args).unwrap().include_tests);
            assert!(
                Cli::try_parse_from(args.into_iter().chain(["--include-tests"]))
                    .unwrap()
                    .include_tests
            );
            assert!(
                !Cli::try_parse_from(args.into_iter().chain(["--ignore-tests"]))
                    .unwrap()
                    .include_tests
            );
            assert!(
                Cli::try_parse_from(
                    args.into_iter()
                        .chain(["--include-tests", "--ignore-tests"])
                )
                .is_err()
            );
        }
    }

    #[test]
    fn backend_options_are_validated_before_loading_models() {
        use clap::{CommandFactory, FromArgMatches};

        for mode in ["exact", "near-miss"] {
            for backend in ["auto", "fastembed", "ollama"] {
                let matches = Cli::command()
                    .try_get_matches_from(["rsdup", "--mode", mode, "--embedding-backend", backend])
                    .unwrap();
                let cli = Cli::from_arg_matches(&matches).unwrap();
                assert!(
                    super::validate_mode_options(&cli, &matches)
                        .unwrap_err()
                        .to_string()
                        .contains("--embedding-backend requires --mode semantic")
                );
            }
        }
        let matches = Cli::command()
            .try_get_matches_from([
                "rsdup",
                "--embedding-backend",
                "fastembed",
                "--embedding-model",
                "local-code",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        assert!(
            super::validate_mode_options(&cli, &matches)
                .unwrap_err()
                .to_string()
                .contains("remove --embedding-model")
        );
        assert_eq!(
            Cli::try_parse_from(["rsdup"]).unwrap().embedding_backend,
            crate::embedding_backend::Backend::Auto
        );
        assert!(Cli::try_parse_from(["rsdup", "--embedding-backend", "unknown"]).is_err());
    }

    #[test]
    fn semantic_arguments_keep_syntax_controls_separate() {
        let args = [
            "rsdup",
            "--mode",
            "semantic",
            "--embedding-model",
            "local-code",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(matches!(cli.mode, ModeArg::Semantic));
        for option in [
            ["--profile", "strict"],
            ["--max-holes", "2"],
            ["--sort", "impact"],
        ] {
            assert!(Cli::try_parse_from(args.into_iter().chain(option)).is_err());
        }
        let cli = Cli::try_parse_from(args.into_iter().chain([
            "--similarity-threshold",
            "0.95",
            "--embedding-cache",
            "target/cache",
            "--max-body-chars",
            "6000",
            "--ignore-tests",
        ]))
        .unwrap();
        assert_eq!(cli.similarity_threshold, Some(0.95));
        assert_eq!(cli.max_body_chars, Some(6000));
        assert!(cli.ignore_tests);
    }

    #[test]
    fn bat_renders_rust_with_line_numbers_without_ansi_when_color_is_disabled() {
        let rendered = highlight_rust("let $1 = value + $2;", false).unwrap();
        assert!(rendered.contains("1 │ let $1 = value + $2;"));
        assert!(!rendered.contains('\u{1b}'));
    }
}
