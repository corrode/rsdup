use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use bat::PrettyPrinter;
use clap::{Parser, ValueEnum};
use rsdup::{DetectionMode, DiscoverOptions, Options, Profile, Report, SortOrder, scan};

#[derive(Debug, Parser)]
#[command(
    name = "rsdup",
    version,
    about = "Find syntax-aware code duplications in Rust"
)]
struct Cli {
    /// Files or directories to scan. Defaults to the current directory.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Minimum number of significant Rust tokens in a duplicate.
    #[arg(short = 'k', long, default_value_t = 40, value_parser = positive_usize)]
    min_tokens: usize,

    /// Minimum number of physical source lines in each occurrence.
    #[arg(short = 'l', long, default_value_t = 5, value_parser = positive_usize)]
    min_lines: usize,

    /// Normalization policy used for structural verification.
    #[arg(long, value_enum, default_value_t = ProfileArg::Balanced)]
    profile: ProfileArg,

    /// Whether nested expression differences may become syntax holes.
    #[arg(long, value_enum, default_value_t = ModeArg::NearMiss)]
    mode: ModeArg,

    /// Maximum syntax holes in a near-miss clone.
    #[arg(long, default_value_t = 1, value_parser = positive_usize)]
    max_holes: usize,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// How duplicate classes are presented in text output.
    #[arg(long, value_enum, default_value_t = View::Summary)]
    view: View,

    /// How duplicate classes are ordered.
    #[arg(long, value_enum, default_value_t = SortArg::Occurrences)]
    sort: SortArg,

    /// Exclude paths matching this simple glob. May be repeated.
    #[arg(long, value_name = "GLOB", action = clap::ArgAction::Append)]
    exclude: Vec<String>,

    /// Do not honor .gitignore, .ignore, or global Git excludes.
    #[arg(long)]
    no_ignore: bool,

    /// Exclude integration tests, test files, and inline test-only code.
    #[arg(long)]
    ignore_tests: bool,

    /// Exit with status 1 when duplicates are found.
    #[arg(long)]
    fail_on_duplicates: bool,

    /// Suppress normal report output.
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
    Exact,
    #[default]
    NearMiss,
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
    let cli = Cli::parse();
    if cli.quiet && matches!(cli.format, Format::Json) {
        bail!("--quiet cannot be combined with --format json");
    }
    if cli.view != View::Summary && matches!(cli.format, Format::Json) {
        bail!("--view only applies to text output");
    }
    if cli.view == View::Interactive {
        bail!("--view interactive is reserved for the future TUI and is not implemented yet");
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
        },
        max_holes: cli.max_holes,
        ignore_tests: cli.ignore_tests,
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

    if !cli.quiet {
        let stdout = io::stdout();
        let colored_output = stdout.is_terminal();
        let mut output = stdout.lock();
        match cli.format {
            Format::Text => match cli.view {
                View::Summary => print_text(&mut output, &report)?,
                View::Syntax => print_syntax(&mut output, &report, colored_output)?,
                View::Interactive => unreachable!("interactive view is rejected before scanning"),
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

fn print_syntax(mut output: impl Write, report: &Report, colored_output: bool) -> Result<()> {
    if report.clones.is_empty() {
        writeln!(
            output,
            "No duplicates found in {} Rust files.",
            report.summary.files
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
        "Found {} clone classes across {} files ({} duplicated lines).",
        report.summary.clone_classes, report.summary.files, report.summary.duplicated_lines
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
            "No duplicates found in {} Rust files.",
            report.summary.files
        )?;
        return Ok(());
    }

    for clone in &report.clones {
        writeln!(output, "{clone}")?;
        for occurrence in &clone.occurrences {
            writeln!(
                output,
                "  {}:{}:{}-{}:{}",
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
        "Found {} clone classes across {} files ({} duplicated lines).",
        report.summary.clone_classes, report.summary.files, report.summary.duplicated_lines
    )
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, ModeArg, highlight_rust};

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
        assert!(matches!(cli.mode, ModeArg::NearMiss));
        assert_eq!(cli.max_holes, 1);
    }

    #[test]
    fn bat_renders_rust_with_line_numbers_without_ansi_when_color_is_disabled() {
        let rendered = highlight_rust("let $1 = value + $2;", false).unwrap();
        assert!(rendered.contains("1 │ let $1 = value + $2;"));
        assert!(!rendered.contains('\u{1b}'));
    }
}
