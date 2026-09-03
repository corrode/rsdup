use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;

#[derive(Clone, Debug, Default)]
pub struct DiscoverOptions {
    pub no_ignore: bool,
    pub excludes: Vec<String>,
}

pub fn discover(paths: &[PathBuf], options: &DiscoverOptions) -> Result<Vec<PathBuf>> {
    let inputs = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    };
    let mut files = BTreeSet::new();

    for input in inputs {
        if !input.exists() {
            bail!("input does not exist: {}", input.display());
        }

        if input.is_file() {
            let root = input.parent().unwrap_or_else(|| Path::new("."));
            if is_rust_file(&input) && !is_excluded(&input, root, &options.excludes) {
                files.insert(normalize_path(&input)?);
            }
            continue;
        }

        let mut builder = WalkBuilder::new(&input);
        builder
            .hidden(false)
            .git_ignore(!options.no_ignore)
            .git_global(!options.no_ignore)
            .git_exclude(!options.no_ignore)
            .ignore(!options.no_ignore)
            .follow_links(false)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                name != ".git" && name != "target"
            });

        for result in builder.build() {
            let entry = result.with_context(|| format!("failed to walk {}", input.display()))?;
            let path = entry.path();
            if entry.file_type().is_some_and(|kind| kind.is_file())
                && is_rust_file(path)
                && !is_excluded(path, &input, &options.excludes)
            {
                files.insert(normalize_path(path)?);
            }
        }
    }

    Ok(files.into_iter().collect())
}

pub fn read_sources(paths: &[PathBuf]) -> Result<Vec<(PathBuf, String)>> {
    paths
        .iter()
        .map(|path| {
            let source = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            Ok((display_path(path), source))
        })
        .collect()
}

fn normalize_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn display_path(path: &Path) -> PathBuf {
    let Ok(cwd) = std::env::current_dir() else {
        return path.to_path_buf();
    };
    path.strip_prefix(cwd).unwrap_or(path).to_path_buf()
}

fn is_rust_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "rs")
}

fn is_excluded(path: &Path, root: &Path, patterns: &[String]) -> bool {
    let supplied = slash_path(path);
    let root_relative = path.strip_prefix(root).ok().map(slash_path);
    let cwd_relative = std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(slash_path));

    patterns.iter().any(|pattern| {
        glob_matches(pattern, &supplied)
            || root_relative
                .as_deref()
                .is_some_and(|path| glob_matches(pattern, path))
            || cwd_relative
                .as_deref()
                .is_some_and(|path| glob_matches(pattern, path))
    })
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    if !pattern.contains('*') {
        return path.contains(&pattern);
    }

    let parts: Vec<_> = pattern.split('*').collect();
    let mut remainder = path;
    let mut first = true;
    for part in &parts {
        if part.is_empty() {
            continue;
        }
        let Some(index) = remainder.find(part) else {
            return false;
        };
        if first && !pattern.starts_with('*') && index != 0 {
            return false;
        }
        remainder = &remainder[index + part.len()..];
        first = false;
    }
    pattern.ends_with('*') || remainder.is_empty()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{DiscoverOptions, discover, glob_matches};

    #[test]
    fn simple_globs_match_paths() {
        assert!(glob_matches("*/generated/*", "/tmp/generated/code.rs"));
        assert!(glob_matches("generated", "/tmp/generated/code.rs"));
        assert!(!glob_matches("*/tests/*", "/tmp/src/code.rs"));
    }

    #[test]
    fn multiple_exclusions_match_paths_relative_to_the_scan_root() {
        let root = tempdir().unwrap();
        for relative in ["src/lib.rs", "generated/code.rs", "vendor/code.rs"] {
            let path = root.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "fn example() {}").unwrap();
        }

        let files = discover(
            &[root.path().to_path_buf()],
            &DiscoverOptions {
                no_ignore: true,
                excludes: vec!["generated/**".into(), "vendor/**".into()],
            },
        )
        .unwrap();

        assert_eq!(
            files,
            vec![root.path().join("src/lib.rs").canonicalize().unwrap()]
        );
    }

    #[test]
    fn exclusions_apply_to_explicit_file_arguments() {
        let root = tempdir().unwrap();
        let file = root.path().join("ignored.rs");
        fs::write(&file, "fn example() {}").unwrap();

        let files = discover(
            &[file],
            &DiscoverOptions {
                no_ignore: true,
                excludes: vec!["ignored.rs".into()],
            },
        )
        .unwrap();

        assert!(files.is_empty());
    }
}
