# rsdup

`rsdup` finds copied Rust code without treating formatting, comments, declarations, or coincidental text as duplication. It parses every file with rust-analyzer's concrete syntax tree and only reports maximal matches made from complete Rust statements.

```console
cargo install --path .
rsdup src
```

The default `near-miss` mode finds exact clones plus statement sequences that differ in at most two conservatively selected nested expression subtrees. Each differing subtree becomes a `$1`, `$2`, and similar syntax hole. Whole statements, top-level callees, nested calls, control flow, assignments, `.await`, `?`, macros, and attributes cannot become holes, keeping the default conservative. Use `--mode exact` for the original exact structural matcher or change the budget with `--max-holes`.

The default `balanced` profile ignores whitespace and comments, alpha-renames unambiguous local variables, and normalizes literal values while preserving their kind. Function names, method names, field names, types, paths, operators, control flow, and macro contents remain semantic anchors outside explicit near-miss holes. Use `--profile strict` to preserve identifiers and literal values unless they occur inside a syntax hole.

```console
rsdup . --min-tokens 40 --min-lines 5
rsdup . --mode exact
rsdup . --max-holes 1
rsdup src tests --profile strict
rsdup . --ignore-tests
rsdup . --exclude 'generated/**' --exclude 'vendor/**'
rsdup . --view syntax
rsdup . --sort impact
rsdup . --format json
rsdup . --fail-on-duplicates
```

Directory scans honor `.gitignore`, `.ignore`, and global Git excludes. The `.git` and `target` directories are always skipped. Explicit `.rs` file arguments are scanned even when ignored.

Pass `--exclude <GLOB>` more than once to omit multiple paths. Patterns are matched against paths relative to each scan root and to the current directory, so `--exclude 'generated/**' --exclude 'fixtures/*.rs'` works with relative or absolute input directories. Unlike ignore files, explicit `--exclude` patterns also apply to `.rs` files named directly on the command line.

Use `--ignore-tests` to exclude integration-test files under `tests/`, conventional `tests.rs`/`*_test.rs` files, `#[cfg(test)]` code, and functions marked with common test attributes such as `#[test]`, `#[tokio::test]`, `#[rstest]`, and `#[wasm_bindgen_test]`. Production code in the same source file is still analyzed.

Text output supports multiple views:

- `--view summary` is the default compact location report.
- `--view syntax` uses bat's Rust syntax highlighter to print each shared template with line numbers and a grid. Syntax holes, normalized locals, and literals appear as `$1`, `$2`, and so on; repeated placeholders represent the same normalized binding. Colors are enabled when output goes to a terminal and disabled when redirected.
- `--view interactive` reserves the interface for a future terminal clone explorer and currently reports that it is not implemented.

Views only affect text output. JSON schema version 2 adds a `holes` count to each clone class; `0` means the class is exact under the selected normalization profile.

Clone classes are ordered with `--sort occurrences` by default. Other modes are:

- `--sort impact`: duplicated volume, calculated as `anchored tokens × (occurrences - 1)`.
- `--sort size`: anchored token count, then physical line count.
- `--sort spread`: number of distinct files containing the clone.
- `--sort path`: the earliest occurrence by path and source position.

Every mode uses deterministic secondary keys, so repeated runs produce stable output.

## How detection works

1. Parse Rust into a full-fidelity concrete syntax tree.
2. Extract executable statement sequences from every block.
3. Remove trivia and conservatively normalize locals and literals.
4. Build a candidate skeleton that preserves statement structure and fixed anchors.
5. Compare candidate pairs recursively and replace only eligible, complete expression subtrees with deterministic holes.
6. Extend candidates to maximal aligned statement ranges without exceeding `--max-holes`.
7. Require `--min-tokens` anchored tokens outside holes and the configured physical line count in every occurrence.
8. Merge occurrences only when they produce the same generalized syntax template.

Candidate skeletons are only an index: a candidate is never accepted without full structural verification. Hidden hole contents do not count toward the token threshold. Near-miss matching does not reorder or insert statements and is not semantic equivalence. `--mode exact` retains rolling token fingerprints followed by exact normalized CST equality.

## Exit status

- `0`: scan completed
- `1`: duplicates found with `--fail-on-duplicates`
- `2`: invalid arguments, unreadable input, or invalid Rust syntax

Run `rsdup --help` for all options.
