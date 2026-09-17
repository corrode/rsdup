# rsdup

`rsdup` finds similar and copied Rust code using rust-analyzer's concrete syntax tree. The default `semantic` mode compares function, closure, and async-block bodies with local embeddings. The `near-miss` and `exact` syntax modes report maximal matches made from complete Rust statements.

```console
cargo install --path . --locked
rsdup src
```

Use `--locked` when installing to use the tested dependency versions in `Cargo.lock`.

`rsdup src` requires a running local Ollama service and an installed embedding model. If exactly one installed model supports embeddings, rsdup selects it automatically; with several, choose one with `--embedding-model`. Install the model separately: rsdup never downloads models or calls a cloud embedding API.

For an immediate offline scan without a model:

```console
rsdup src --mode near-miss
```

## Choosing a mode

| Mode | Advantages and when to prefer it | Limitations |
| --- | --- | --- |
| `semantic` (default) | Broad similarity search across function, closure, and async-block bodies, even with changed calls, types, or setup steps. Prefer it for exploratory review. | Fuzzy, model-dependent candidates, not equivalent behavior. Requires a local embedding service, adds service latency, and compares pairs in quadratic time. |
| `near-miss` | Deterministic, offline matching of complete statements with small, conservative expression holes. Prefer it for repeatable copy-paste checks that allow limited variation. | Misses inserted or reordered statements and changed fixed anchors. |
| `exact` | Deterministic, offline, strictest structural matching under the selected normalization profile. Prefer it when holes would be too permissive. | Misses structural edits. The default `balanced` normalization still ignores some name and literal differences; this is neither byte equality nor behavioral equivalence. |

Use semantic results as a review queue. For CI, choose a syntax mode explicitly:

```console
rsdup src --mode near-miss --fail-on-duplicates
rsdup src --mode exact --profile strict --fail-on-duplicates
```

In semantic mode, `--fail-on-duplicates` means candidate groups were found, not proven duplicates. No mode proves that a refactor is safe; review behavior and context before extracting shared code.

### Syntax normalization and holes

`near-miss` finds exact clones plus statement sequences that differ in at most one conservatively selected nested expression subtree by default. Each differing subtree becomes a `$1`, `$2`, and similar syntax hole. Whole statements, top-level callees, nested calls, control flow, assignments, `.await`, `?`, macros, and attributes cannot become holes. Change the budget with `--mode near-miss --max-holes`.

In both syntax modes, the default `balanced` profile ignores whitespace and comments, alpha-renames unambiguous local variables, and normalizes literal values while preserving their kind. Function names, method names, field names, types, paths, operators, control flow, and macro contents remain fixed anchors outside explicit near-miss holes. Use `--mode exact --profile strict` to preserve identifiers and literal values; in near-miss mode, they can still differ inside a syntax hole.

## Scanning and output

```console
rsdup . --min-tokens 40 --min-lines 5
rsdup . --mode exact
rsdup . --mode near-miss --max-holes 2
rsdup src tests --mode exact --profile strict --include-tests
rsdup . --include-tests
rsdup . --exclude 'generated/**' --exclude 'vendor/**'
rsdup . --view syntax
rsdup . --mode near-miss --sort impact
rsdup . --format json
rsdup . --fail-on-duplicates
```

Directory scans honor `.gitignore`, `.ignore`, global Git excludes, and hidden-path conventions. This prevents nested agent or Git workspaces such as `.workspace/`, `.worktrees/`, and `.claude/worktrees/` from being scanned as extra copies of the project. The `.git` and `target` directories are always skipped. Explicit files or hidden directories passed as input paths are still scanned.

Pass `--exclude <GLOB>` more than once to omit multiple paths. Patterns are matched against paths relative to each scan root and to the current directory, so `--exclude 'generated/**' --exclude 'fixtures/*.rs'` works with relative or absolute input directories. Unlike ignore files, explicit `--exclude` patterns also apply to `.rs` files named directly on the command line.

Tests are excluded by default in every detection mode: files under any `test/` or `tests/` directory, conventional `tests.rs`/`*_test.rs` files, `#[cfg(test)]` code, and functions marked with common test attributes such as `#[test]`, `#[tokio::test]`, `#[rstest]`, and `#[wasm_bindgen_test]`. Production code in the same source file is still analyzed. Pass `--include-tests` to opt in to test code. The old `--ignore-tests` flag remains accepted for compatibility but is no longer needed; it cannot be combined with `--include-tests`. Library options also exclude tests by default; set `ignore_tests: false` to include them.

Text output supports multiple views:

- `--view summary` is the default: numbered groups and clickable `path:line` locations, without internal hashes or column ranges.
- `--view syntax` uses bat's Rust syntax highlighter with line numbers and a grid. In semantic mode it shows original bodies and matched pairs. In syntax modes it prints shared templates: syntax holes, normalized locals, and literals appear as `$1`, `$2`, and so on; repeated placeholders represent the same normalized binding. Colors are enabled when output goes to a terminal and disabled when redirected.
- `--view interactive` opens a two-pane terminal browser with a searchable match list, highlighted original source, and editor navigation. See [Interactive browser](#interactive-browser).

Views only affect text output. Syntax JSON schema version 2 includes a `holes` count in each clone class; `0` means the class is exact under the selected normalization profile. Semantic JSON uses a separate schema version 1. Scripts that expect syntax schema version 2 must now pass `--mode exact` or `--mode near-miss` explicitly; plain `--format json` uses the semantic default.

### Interactive browser

```console
rsdup . --view interactive
rsdup . --view interactive --editor 'zed --wait'
rsdup . --view interactive --theme catppuccin-mocha
rsdup . --mode near-miss --view interactive
```

The left column lists match groups. The source pane shows the focused occurrence and a second snippet when there is enough vertical space. Surrounding file context is included by default: each card starts up to three lines before its match, and scrolling continues above or below it through the file. Lines outside that occurrence's matched range are dimmed; matched lines keep their Rust syntax colors and diff highlights. Dimming is line-based, so a partially matched line remains bright. No separate file-view mode or command language is needed. Each card includes its file location, actual source line numbers, and pair-specific evidence for semantic matches. Syntax matches show original source rather than the normalized template. Semantic scores do not imply equivalence, and connected groups need not match pairwise.

Difference highlighting is on by default. The focused occurrence is compared with the next occurrence in the group, wrapping at the end. Red marks lines only in the first snippet, green marks lines only in the second, and amber marks changed lines on both sides; gutter `−`/`+` markers identify the side without relying on color. Rust syntax colors and real source line numbers remain visible. The footer names the comparison pair, including when the terminal only has room for one card. Press `n`/`p` to change pairs or `d` to toggle highlighting.

The diff compares exact text, including whitespace and line endings—not normalized Rust or semantic behavior. Inserted lines are aligned so they do not mark every following line as changed. Work is bounded for large snippets: a `LIMIT` label means the unmatched middle is conservatively highlighted rather than a minimal diff. Only the current pair is cached; redraws do not recompute it. A pair without a direct similarity edge is explicitly labeled as such.

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Move through matches, or scroll source and its surrounding context |
| `Tab`, `h` / `l` | Switch between match-list and source focus |
| `n` / `p`, `]` / `[` | Next / previous occurrence and comparison pair in the selected group |
| `d` | Toggle difference highlighting (on by default) |
| `t` / `T` | Next / previous color theme |
| `PgUp` / `PgDn`, `g` / `G` | Page or jump to the beginning/end of the focused pane |
| `H` / `L`, `Shift-Left` / `Shift-Right` | Scroll source horizontally; line numbers stay fixed |
| `/` | Filter groups by function name or path; Enter keeps the filter, Esc clears it |
| `Enter` / `o` | Open the focused occurrence in your editor |
| `?` | Show keyboard help |
| `q` / `Ctrl-C` | Quit |

Themes come from [`ratatui-themes`](https://docs.rs/ratatui-themes/latest/ratatui_themes/), including Dracula (the default), Nord, Catppuccin, Gruvbox, Tokyo Night, and Solarized. Run `rsdup --list-themes` for the full catalog; this does not scan files or contact Ollama. Choose with `--theme NAME`, or cycle with `t`/`T` while browsing. The active theme appears in the header. Palettes apply throughout the browser, including syntax highlighting, dimmed context, and diff tints. Switching preserves your selection and scroll position and recolors cached source without rereading files or requesting embeddings. Theme changes are session-only; pass `--theme` again to reuse a favorite. This option only affects the interactive view, not text or JSON reports.

Editor selection uses `--editor`, then `VISUAL`, then `EDITOR`, falling back to `zed`. Zed, VS Code/Code Insiders/VSCodium, Vim/Neovim/Vi, Emacs/emacsclient, and Nano receive the source location; other executables receive the file path only. Commands support quoted arguments but are executed directly, without shell expansion. Use an editor-specific wait option if you want the browser to wait for a GUI editing session. Terminal editors temporarily take over the terminal; the browser resumes afterward, including after launch failures.

Scanning completes before the browser opens. Browsing makes no embedding requests, shares cached source and highlighting across occurrences from the same file, and redraws only for input or resize events. Use `--embedding-cache target/rsdup-embeddings` to speed up subsequent semantic scans. The browser requires terminal input/output and at least 60 columns by 12 rows; a taller terminal shows two snippets at once. It cannot be combined with `--quiet` or JSON output.

This is a read-only snapshot, not a live editor. Both syntax and semantic snippets retain the scanned match. Context is loaded once when the browser opens and is only shown if the matched text still agrees with the scan. Missing or changed matches, unreadable files, and files over the 2 MiB context limit fall back to the scanned snippet with a warning. Edits made while browsing do not update matches or locations: rerun the scan after refactoring.

### Sorting (syntax modes only)

Syntax clone classes are ordered with `--sort occurrences` by default. Other orders are:

- `--sort impact`: duplicated volume, calculated as `anchored tokens × (occurrences - 1)`.
- `--sort size`: anchored token count, then physical line count.
- `--sort spread`: number of distinct files containing the clone.
- `--sort path`: the earliest occurrence by path and source position.

Every syntax sort order uses deterministic secondary keys, so repeated runs produce stable output.

## Semantic similarity (default)

`--mode semantic` compares function, closure, and async-block bodies with an installed **local Ollama** embedding model. It is independent of the syntax detector: changed calls, inserted setup steps, and different types can still produce a useful comparison. A high score is not proof of equivalent behavior or a recommendation to extract a helper.

```console
rsdup . --mode semantic
rsdup . --mode semantic --embedding-model unclemusclez/jina-embeddings-v2-base-code --embedding-cache target/rsdup-embeddings --format json
rsdup . --mode semantic --embedding-model unclemusclez/jina-embeddings-v2-base-code --similarity-threshold 0.95 --max-body-chars 6000 --view syntax
```

Start Ollama and install your chosen embedding model separately. When exactly one installed model supports embeddings, rsdup selects it automatically and prints its name. If several do, rsdup lists them and asks you to choose with `--embedding-model`; chat-only models are not selected. rsdup only connects to `http://localhost:11434`; it disables proxies and redirects and never downloads a model or calls a hosted embedding API. There are no model calls in `near-miss` or `exact` mode. Semantic scans use CPU/GPU resources on the Ollama server. Keep the model tag unchanged during a scan: rsdup checks its digest before and after uncached requests and before publishing the report.

### Extraction and evidence

- Bodies exclude their own function signature or closure parameters. Original signatures/context and body source, including comments, remain available for review. Comments are blanked token-wise for embedding; string contents are preserved.
- `--min-tokens` and `--min-lines` apply to each complete body (defaults: 40 tokens, 5 physical lines). `--max-body-chars` limits the comment-free input to 6,000 Unicode characters by default. Character limits cannot guarantee a model's token limit: oversized provider requests fail rather than being silently truncated. Lower the character limit if needed.
- Extraction does not expand macros, resolve names, or evaluate `cfg`. It does not extract every control-flow block or match arm. Size and test exclusions are counted separately in the report.
- Default test exclusion shares the syntax detector's conservative test heuristics (disabled with `--include-tests`): conventional test paths/attributes and any `cfg` attribute containing the identifier `test`. This also excludes `cfg(not(test))` and `cfg(any(test, feature = "…"))`; it is not Boolean `cfg` evaluation. A body containing test-attributed descendants is excluded as a whole to keep test code out of its embedding. Production siblings remain eligible. Both `test/` and `tests/` directory components are excluded, including nested paths such as `src/adapters/test/`; similarly named directories such as `test_utils/` are not excluded by this path rule.
- Body identity hashes Rust token kinds and text, ignoring trivia but preserving names and literal values. `"a b"` and `"a  b"` are different. `exact_body` means equality under this token identity, not semantic equivalence or the balanced syntax profile.
- Equal token bodies share one embedding, choosing the lexicographically smallest comment-free input. All occurrences retain their locations. JSON records the exact representative's input digest as well as the token-body hash.

The default semantic summary shows numbered groups, function names, test markers, and `path:line` locations. Similarity ranges summarize matched pairs only; groups where not every pair matched are explicitly marked. Internal IDs, full signatures, and individual edges stay out of the summary.

`--view syntax` shows each original body, not a generalized syntax template, followed by matched pairs using local location numbers. This detailed view and JSON expose the actual pairwise cosine edges. Similarity scores are not confidence values; there is no distance boost or automated refactoring-priority score. `--profile`, `--max-holes`, and `--sort` are syntax-only controls and cannot be used in semantic mode.

### Groups and reproducibility

All eligible pairs are compared by raw cosine similarity (default threshold: `0.93`). Invalid, zero, nonfinite, or inconsistent-dimension vectors fail the scan. Same-file overlapping bodies cannot form an edge. Within each connected component, overlapping members are removed deterministically by strongest incident edge, token count, then identity; components are then rebuilt from the surviving edges. A chain A–B–C does **not** imply an A–C match. The JSON retains all eligible units, including isolated and overlap-suppressed units.

Semantic JSON has its own schema (`mode: "semantic"`, `schema_version: 1`), leaving syntax JSON version 2 unchanged. It includes extraction/filtering policy, model digest/dimensions, source context, one-based start and exclusive end positions, literal-sensitive identities, input digests, and supporting edges. Edge endpoints and group members index the top-level `units` array. Group IDs depend on retained body identities, paths, kinds, and multiplicity—not source line numbers or similarity scores. Groups are deterministically ordered by source traversal, not by refactoring value.

`--embedding-cache DIR` opts into persistent caching. Entries are keyed by exact input, provider/model identity and digest, and cache-policy version; no source text is stored. Writes are atomic, invalid entries cause actionable errors, and successful requests survive an interrupted scan. Cached vectors can still be sensitive derived data: keep them private. Reports contain original source, so keep reports private too. Neither needs to be committed. Ollama must remain available even on a cache-only run to verify model identity.

Comparison time is quadratic in eligible occurrences; memory holds vectors plus accepted edges, not a dense similarity matrix. Narrow scan paths, use exclusions, or raise size thresholds for very large repositories. Results depend on the model and extraction policy; a score or cluster count is not a portable golden test. Offline tests use synthetic bodies and vectors rather than customer code or a live model.

`--fail-on-duplicates` exits with status 1 for semantic **candidate groups**, not proven duplicates. Operational/provider errors exit with status 2.

## Library usage

The CLI default does not change the library APIs: `scan` and `analyze_sources` remain syntax-only, and `Options::default()` still selects near-miss matching. Semantic analysis uses the separate `semantic::analyze` or `scan_semantic` APIs, where callers explicitly supply a `semantic::Embedder`. The library does not automatically select Ollama; callers can use another embedder without it.

## How syntax detection works

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
- `1`: syntax duplicates or semantic candidate groups found with `--fail-on-duplicates`
- `2`: invalid arguments, unreadable input, invalid Rust syntax, or operational/provider failures (including Ollama connection, model selection, or embedding errors)

Run `rsdup --help` for all options.
