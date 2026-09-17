//! Launch an editor directly, without a shell, after the TUI has been suspended.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

/// Open an existing regular file and wait for the editor process to exit.
///
/// Command precedence is `command` (the explicit `--editor` value), `VISUAL`,
/// `EDITOR`, then `zed`. A present but empty, non-Unicode, or malformed value is
/// an error, not a request to fall back. Shell-style quotes and backslashes are
/// parsed with `shlex`; no shell is invoked and no variables, tildes, globs, or
/// command substitutions are expanded. Existing command options are preserved.
///
/// Zed, VS Code variants, Vim variants, Emacs variants, and Nano receive a
/// location; unknown executables receive only the file path. Executable names
/// with an `.exe` suffix are recognized too. Input lines and Unicode scalar
/// columns are one-based; zero is clamped to one. Vim columns are converted to
/// UTF-8 bytes and VS Code columns to UTF-16 code units by reading the source
/// line. Read/UTF-8 errors and missing lines are reported; columns beyond the
/// line are clamped to its end. Other editors receive the scalar column unchanged;
/// their column conventions are not normalized here.
/// Paths are canonicalized before launch, both to remain
/// independent of editor working-directory changes and to avoid leading-dash
/// option injection. Missing files and directories are rejected.
///
/// The caller must suspend the TUI before calling and restore it afterward,
/// including on error. Standard streams are inherited so terminal editors work.
/// This waits for the launched process, not necessarily a GUI editing session;
/// configure an editor-specific wait flag if desired (for example `zed --wait`).
pub(crate) fn open(path: &Path, line: usize, column: usize, command: Option<&str>) -> Result<()> {
    let visual = env::var_os("VISUAL");
    let editor = env::var_os("EDITOR");
    let command = select_command(command, visual.as_deref(), editor.as_deref())?;
    let path = canonical_file(path)?;
    let words = parse_command(command)?;
    let column = editor_column(&path, line, column, &words[0])?;
    let (program, args) = build_command(&path, line, column, &words);
    let status = Command::new(&program)
        .args(&args)
        .status()
        .with_context(|| {
            format!(
                "failed to launch editor {program:?}; check --editor, VISUAL, or EDITOR and ensure the executable is installed"
            )
        })?;
    if !status.success() {
        bail!(
            "editor {program:?} exited with {status}; check its arguments in --editor, VISUAL, or EDITOR"
        );
    }
    Ok(())
}

// Environment values are passed in so selection can be tested without mutating
// process-global environment state (or depending on the user's configuration).
fn select_command<'a>(
    explicit: Option<&'a str>,
    visual: Option<&'a OsStr>,
    editor: Option<&'a OsStr>,
) -> Result<&'a str> {
    let (source, value) = if let Some(value) = explicit {
        ("--editor", OsStr::new(value))
    } else if let Some(value) = visual {
        ("VISUAL", value)
    } else if let Some(value) = editor {
        ("EDITOR", value)
    } else {
        return Ok("zed");
    };
    let value = value
        .to_str()
        .with_context(|| format!("{source} must contain a Unicode editor command"))?;
    if value.trim().is_empty() {
        bail!("{source} is empty; provide an editor executable or unset it");
    }
    Ok(value)
}

fn canonical_file(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve editor file {}", path.display()))?;
    let metadata = canonical
        .metadata()
        .with_context(|| format!("cannot inspect editor file {}", canonical.display()))?;
    if !metadata.is_file() {
        bail!("editor path is not a regular file: {}", canonical.display());
    }
    Ok(canonical)
}

fn parse_command(command: &str) -> Result<Vec<String>> {
    let words = shlex::split(command)
        .context("invalid editor command: unmatched quote or trailing escape")?;
    let Some(program) = words.first() else {
        bail!("editor command is empty; provide an executable");
    };
    if program.is_empty() {
        bail!("editor executable is empty; provide an executable");
    }
    if words.iter().any(|word| word.contains('\0')) {
        bail!("editor command contains a NUL byte");
    }
    Ok(words)
}

fn editor_basename(program: &str) -> &str {
    let basename = Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program);
    basename.strip_suffix(".exe").unwrap_or(basename)
}

/// Convert scalar columns only for editors whose units are known here.
fn editor_column(path: &Path, line: usize, column: usize, program: &str) -> Result<usize> {
    let utf16 = match editor_basename(program) {
        "vim" | "nvim" | "vi" => false,
        "code" | "code-insiders" | "codium" => true,
        _ => return Ok(column.max(1)),
    };
    let line = line.max(1);
    let file =
        File::open(path).with_context(|| format!("cannot read editor file {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut text = String::new();
    for _ in 0..line {
        text.clear();
        let bytes = reader.read_line(&mut text).with_context(|| {
            format!(
                "cannot read UTF-8 source for editor at {}:{line}",
                path.display()
            )
        })?;
        if bytes == 0 {
            bail!(
                "editor source line {line} does not exist in {}",
                path.display()
            );
        }
    }
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    // Combining marks count separately, just as they do in rsdup positions.
    let units: usize = text
        .chars()
        .take(column.max(1) - 1)
        .map(|character| {
            if utf16 {
                character.len_utf16()
            } else {
                character.len_utf8()
            }
        })
        .sum();
    Ok(units + 1)
}

/// Pure command construction. `words` must come from `parse_command`, `path`
/// must be canonical, and `column` must already use the editor's column units.
fn build_command(
    path: &Path,
    line: usize,
    column: usize,
    words: &[String],
) -> (String, Vec<OsString>) {
    let program = &words[0];
    let options = &words[1..];
    let basename = editor_basename(program);
    let line = line.max(1);
    let column = column.max(1);
    let mut args: Vec<OsString> = options.iter().map(OsString::from).collect();
    match basename {
        "zed" | "code" | "code-insiders" | "codium" => {
            if basename != "zed" {
                args.push("--goto".into());
            }
            let mut location = path.as_os_str().to_os_string();
            location.push(format!(":{line}:{column}"));
            args.push(location);
        }
        "vim" | "nvim" | "vi" => {
            // Only formatted integers enter the editor's command language.
            args.push(format!("+call cursor({line},{column})").into());
            args.push(path.as_os_str().to_os_string());
        }
        "emacs" | "emacsclient" => {
            args.push(format!("+{line}:{column}").into());
            args.push(path.as_os_str().to_os_string());
        }
        "nano" => {
            args.push(format!("+{line},{column}").into());
            args.push(path.as_os_str().to_os_string());
        }
        _ => args.push(path.as_os_str().to_os_string()),
    }
    (program.clone(), args)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keep parsing and pure construction together for command syntax tests.
    fn build_command(
        path: &Path,
        line: usize,
        column: usize,
        command: &str,
    ) -> Result<(String, Vec<OsString>)> {
        Ok(super::build_command(
            path,
            line,
            column,
            &parse_command(command)?,
        ))
    }

    #[test]
    fn scalar_columns_convert_to_editor_units() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), "first line\ncafé😀e\u{301}X\r\n")?;
        // X is scalar column 8, byte column 13, and UTF-16 column 9.
        for name in ["vim", "nvim", "vi", "code", "code-insiders", "codium"] {
            for suffix in ["", ".exe"] {
                let program = format!("/editors/{name}{suffix}");
                let vim = matches!(name, "vim" | "nvim" | "vi");
                for (scalar, bytes, utf16) in [
                    (1, 1, 1),
                    (4, 4, 4),
                    (5, 6, 5),
                    (6, 10, 7),
                    (7, 11, 8),
                    (8, 13, 9),
                    (9, 14, 10),
                    (usize::MAX, 14, 10),
                ] {
                    assert_eq!(
                        editor_column(file.path(), 2, scalar, &program)?,
                        if vim { bytes } else { utf16 },
                        "{program}, scalar {scalar}"
                    );
                }
                let column = editor_column(file.path(), 2, 8, &program)?;
                let words = parse_command(&format!("'{program}' --existing-option"))?;
                let (_, args) = super::build_command(file.path(), 2, column, &words);
                assert_eq!(args[0], "--existing-option");
                if vim {
                    assert_eq!(args[1], "+call cursor(2,13)");
                } else {
                    assert_eq!(args[1], "--goto");
                    let mut expected = file.path().as_os_str().to_os_string();
                    expected.push(":2:9");
                    assert_eq!(args[2], expected);
                }
                assert_eq!(editor_column(file.path(), 0, 0, &program)?, 1);
            }
        }
        Ok(())
    }

    #[test]
    fn conversion_reports_source_errors_without_affecting_other_editors() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.rs");
        for program in ["vim", "code"] {
            assert!(editor_column(&path, 1, 1, program).is_err());
        }
        for program in ["zed", "emacs", "emacsclient", "nano", "other"] {
            assert_eq!(editor_column(&path, 1, 8, program)?, 8);
        }
        std::fs::write(&path, b"valid\n\xff\n")?;
        for program in ["vim", "code"] {
            assert!(editor_column(&path, 2, 1, program).is_err());
        }
        std::fs::write(&path, "\r\ncafé")?;
        for program in ["vim", "code"] {
            assert_eq!(editor_column(&path, 1, 100, program)?, 1);
            assert_eq!(editor_column(&path, 2, 4, program)?, 4);
            assert!(editor_column(&path, 3, 1, program).is_err());
        }
        Ok(())
    }

    #[test]
    fn selection_is_explicit_then_visual_then_editor_then_zed() -> Result<()> {
        let visual = Some(OsStr::new("vim -f"));
        let editor = Some(OsStr::new("nano"));
        assert_eq!(select_command(Some("code"), visual, editor)?, "code");
        assert_eq!(select_command(None, visual, editor)?, "vim -f");
        assert_eq!(select_command(None, None, editor)?, "nano");
        assert_eq!(select_command(None, None, None)?, "zed");
        assert!(select_command(Some("  "), visual, editor).is_err());
        assert!(select_command(None, Some(OsStr::new("")), editor).is_err());
        assert!(select_command(None, None, Some(OsStr::new("\t"))).is_err());
        Ok(())
    }

    #[test]
    fn known_editors_preserve_options_and_map_locations() -> Result<()> {
        let path = Path::new("/project/a file.rs");
        let located = "/project/a file.rs:12:34";
        for (name, location_args) in [
            ("zed", vec![located]),
            ("code", vec!["--goto", located]),
            ("code-insiders", vec!["--goto", located]),
            ("codium", vec!["--goto", located]),
            ("vim", vec!["+call cursor(12,34)", "/project/a file.rs"]),
            ("nvim", vec!["+call cursor(12,34)", "/project/a file.rs"]),
            ("vi", vec!["+call cursor(12,34)", "/project/a file.rs"]),
            ("emacs", vec!["+12:34", "/project/a file.rs"]),
            ("emacsclient", vec!["+12:34", "/project/a file.rs"]),
            ("nano", vec!["+12,34", "/project/a file.rs"]),
            ("other", vec!["/project/a file.rs"]),
        ] {
            for suffix in ["", ".exe"] {
                let executable = format!("/editors/{name}{suffix}");
                let (program, args) =
                    build_command(path, 12, 34, &format!("{executable} --existing-option"))?;
                assert_eq!(program, executable);
                let expected: Vec<OsString> = std::iter::once("--existing-option")
                    .chain(location_args.iter().copied())
                    .map(OsString::from)
                    .collect();
                assert_eq!(args, expected, "{name}{suffix}");
            }
        }
        Ok(())
    }

    #[test]
    fn quotes_escapes_and_metacharacters_are_arguments_not_shell_code() -> Result<()> {
        let path = Path::new("/project/-file; $(touch nope).rs");
        let (program, args) = build_command(
            path,
            2,
            3,
            r#"'/Applications/My Editor' "two words" escaped\ space '' '$HOME' ${EDITOR} ~/bin *.rs ';' '|' '>' '$(touch nope)' '`whoami`'"#,
        )?;
        assert_eq!(program, "/Applications/My Editor");
        let mut expected: Vec<OsString> = [
            "two words",
            "escaped space",
            "",
            "$HOME",
            "${EDITOR}",
            "~/bin",
            "*.rs",
            ";",
            "|",
            ">",
            "$(touch nope)",
            "`whoami`",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        expected.push(path.into());
        assert_eq!(args, expected);
        assert_eq!(build_command(path, 1, 1, "$EDITOR")?.0, "$EDITOR");
        Ok(())
    }

    #[test]
    fn invalid_commands_are_rejected() {
        for command in [
            "",
            "  ",
            "''",
            "\"\" --wait",
            "'vim",
            "\"vim",
            "vim \\",
            "vim\0",
        ] {
            assert!(
                build_command(Path::new("/file.rs"), 1, 1, command).is_err(),
                "{command:?}"
            );
        }
    }

    #[test]
    fn zero_coordinates_are_clamped() -> Result<()> {
        let (_, args) = build_command(Path::new("/file.rs"), 0, 0, "vim")?;
        assert_eq!(args[0], "+call cursor(1,1)");
        let (_, args) = build_command(Path::new("/file.rs"), 0, 0, "zed")?;
        assert_eq!(args[0], "/file.rs:1:1");
        Ok(())
    }

    #[test]
    fn canonicalization_rejects_missing_files_and_directories() -> Result<()> {
        let directory = tempfile::tempdir()?;
        assert!(canonical_file(&directory.path().join("missing.rs")).is_err());
        assert!(canonical_file(directory.path()).is_err());
        let file = directory.path().join("-file with spaces.rs");
        std::fs::write(&file, "")?;
        let canonical = canonical_file(&file)?;
        assert!(canonical.is_absolute());
        assert_eq!(canonical, file.canonicalize()?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_paths_are_preserved_and_commands_rejected() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(b"/file-\xff.rs".to_vec()));
        let (_, args) = build_command(&path, 1, 2, "zed")?;
        let mut expected = path.into_os_string();
        expected.push(":1:2");
        assert_eq!(args, vec![expected]);
        let invalid = OsString::from_vec(vec![0xff]);
        assert!(select_command(None, Some(&invalid), None).is_err());
        assert_eq!(select_command(Some("vim"), Some(&invalid), None)?, "vim");
        Ok(())
    }
}
