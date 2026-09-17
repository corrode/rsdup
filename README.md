# rsdup

`rsdup` finds similar and copied Rust code using rust-analyzer's concrete syntax tree. The default `semantic` mode compares function, closure, and async-block bodies with local embeddings. The `near-miss` and `exact` syntax modes report maximal matches made from complete Rust statements.

```console
cargo install --path . --locked
rsdup src
```

Use `--locked` when installing to use the tested dependency versions in `Cargo.lock`.

`rsdup src` first looks for a usable local Ollama embedding model. If exactly one is installed, it uses that model automatically. Otherwise, it asks you to choose **FastEmbed** (in-process CPU inference, with a ~642 MB model download on first use) or **Ollama** (setup instructions for your local service). Your source stays on your machine. Choices apply to the current run; use `--embedding-backend fastembed` or `--embedding-backend ollama` to choose explicitly. With several Ollama models, select one using `--embedding-model`.

Prompts require terminal stdin and stderr and are disabled by `--quiet`. In scripts, select a backend explicitly; auto mode fails with setup instructions if Ollama cannot be selected. Explicit FastEmbed selection authorizes downloading its pinned public model if missing. Model download progress and backend messages go to stderr, leaving JSON output clean.

For an immediate offline scan without a model:

```console
rsdup src --mode near-miss
```

## Choosing a mode

| Mode | Advantages and when to prefer it | Limitations |
| --- | --- | --- |
| `semantic` (default) | Broad similarity search across function, closure, and async-block bodies, even with changed calls, types, or setup steps. Prefer it for exploratory review. | Fuzzy, model-dependent candidates, not equivalent behavior. Requires local model weights, consumes CPU/GPU resources, and compares pairs in quadratic time. FastEmbed CPU can be slower than GPU-backed Ollama. |
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

The left column lists match groups, sorted by occurrence count (highest first). Each card shows the shared directory once, then groups two occurrence rows by file in a Unicode tree. Shared paths are muted; differing file suffixes and function names stand out. Paths under the current directory are shown relative to it, and long paths are shortened with explicit ellipses. Selection adds a subtle background without expanding the card or replacing its text colors. Use `n`/`p` to reveal further occurrences in stable pairs. The source pane shows the focused occurrence and a second snippet when there is enough vertical space. Surrounding file context is included by default: each card starts up to three lines before its match, and scrolling continues above or below it through the file. Lines outside that occurrence's matched range are dimmed; matched lines keep their Rust syntax colors and diff highlights. Dimming is line-based, so a partially matched line remains bright. No separate file-view mode or command language is needed. Each card includes its file location, actual source line numbers, and pair-specific evidence for semantic matches. Syntax matches show original source rather than the normalized template. Semantic scores do not imply equivalence, and connected groups need not match pairwise.

Difference highlighting is on by default. The focused occurrence is compared with the next occurrence in the group, wrapping at the end. Red marks lines only in the first snippet, green marks lines only in the second, and amber marks changed lines on both sides; gutter `−`/`+` markers identify the side without relying on color. Rust syntax colors and real source line numbers remain visible. The footer names the comparison pair, including when the terminal only has room for one card. Press `n`/`p` to change pairs or `d` to toggle highlighting.

The diff compares exact text, including whitespace and line endings—not normalized Rust or semantic behavior. Inserted lines are aligned so they do not mark every following line as changed. Work is bounded for large snippets: a `LIMIT` label means the unmatched middle is conservatively highlighted rather than a minimal diff. Only the current pair is cached; redraws do not recompute it. A pair without a direct similarity edge is explicitly labeled as such.

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Move through matches, or scroll source and its surrounding context |
| `Tab`, `h` / `l` | Switch between match-list and source focus |
| `n` / `p`, `]` / `[` | Next / previous occurrence and comparison pair in the selected group |
| `d` | Toggle difference highlighting (on by default) |
| `t` | Open the theme picker; type to filter |
| `s` | Choose match sorting: occurrences, matched lines, distinct files, or path |
| `PgUp` / `PgDn`, `g` / `G` | Page or jump to the beginning/end of the focused pane |
| `H` / `L`, `Shift-Left` / `Shift-Right` | Scroll source horizontally; line numbers stay fixed |
| `/` | Filter groups by function name or path; Enter keeps the filter, Esc clears it |
| `Enter` / `o` | Open the focused occurrence in your editor |
| `y` | Copy the focused occurrence’s relative `path:line` |
| `?` | Show keyboard help |
| `q` / `Ctrl-C` | Quit |

Press `s` to choose another ordering with arrows or `Ctrl-N`/`Ctrl-P`; Enter applies and Esc cancels. Sorting keeps the selected match, occurrence, and source position, and remains active when filtering. Matched-line counts sum all occurrence ranges, and file counts measure distinct paths; neither estimates removable code. The browser starts with occurrence sorting independently of the syntax report's `--sort` option. Text and JSON ordering are unchanged.

Themes come from [`ratatui-themes`](https://docs.rs/ratatui-themes/latest/ratatui_themes/), including Dracula (the default), Nord, Catppuccin, Gruvbox, Tokyo Night, and Solarized. Run `rsdup --list-themes` for the full catalog; this does not scan files or contact Ollama. Choose with `--theme NAME`, or press `t` while browsing to open the theme picker. Type to filter by name and move with arrows or `Ctrl-N`/`Ctrl-P`. Each selection previews immediately across the interface, including syntax highlighting, with the code still visible behind a lightly dimmed modal background. Enter keeps the preview; Esc restores the original theme, marked with `*` in the list. The active theme appears in the header. Palettes apply throughout the browser, including syntax highlighting, dimmed context, and diff tints. Switching preserves your selection and scroll position and recolors cached source without rereading files or requesting embeddings. Theme changes are session-only; pass `--theme` again to reuse a favorite. This option only affects the interactive view, not text or JSON reports.

Press `y` to copy a location such as `src/parser.rs:42`. The path is relative to the working directory; the line is the focused occurrence’s starting line, unchanged by scrolling. Copying uses [`arboard`](https://docs.rs/arboard/) on macOS, Windows, and Linux (X11 and supported Wayland compositors), without external clipboard commands. If no clipboard is available, the interface reports the failure. On Linux, clipboard contents may stop being available after rsdup exits unless a clipboard manager retains them.

Editor selection uses `--editor`, then `VISUAL`, then `EDITOR`, falling back to `zed`. Zed, VS Code/Code Insiders/VSCodium, Vim/Neovim/Vi, Emacs/emacsclient, and Nano receive the source location; other executables receive the file path only. Commands support quoted arguments but are executed directly, without shell expansion. Use an editor-specific wait option if you want the browser to wait for a GUI editing session. Terminal editors temporarily take over the terminal; the browser resumes afterward, including after launch failures.

Scanning completes before the browser opens. Browsing makes no embedding requests, shares cached source and highlighting across occurrences from the same file, and redraws only for input or resize events. Use `--embedding-cache target/rsdup-embeddings` to speed up subsequent semantic scans. The browser requires terminal input/output and at least 60 columns by 12 rows; a taller terminal shows two snippets at once. It cannot be combined with `--quiet` or JSON output.

The displayed code comes from the scan. Context is loaded once when the browser opens and is only shown if the matched text still agrees with the scan. Missing or changed matches, unreadable files, and files over the 2 MiB context limit fall back to the scanned snippet with a warning. Edits made while browsing do not update matches or locations: rerun the scan after refactoring.

### Sorting (syntax modes only)

Syntax clone classes are ordered with `--sort occurrences` by default. Other orders are:

- `--sort impact`: duplicated volume, calculated as `anchored tokens × (occurrences - 1)`.
- `--sort size`: anchored token count, then physical line count.
- `--sort spread`: number of distinct files containing the clone.
- `--sort path`: the earliest occurrence by path and source position.

Every syntax sort order uses deterministic secondary keys, so repeated runs produce stable output.

## Semantic similarity (default)

`--mode semantic` compares function, closure, and async-block bodies using **local embeddings**, via Ollama or FastEmbed. It is independent of the syntax detector: changed calls, inserted setup steps, and different types can still produce a useful comparison. A high score is not proof of equivalent behavior or a recommendation to extract a helper.

```console
rsdup . --mode semantic
rsdup . --embedding-backend fastembed
rsdup . --embedding-backend ollama
rsdup . --mode semantic --embedding-model unclemusclez/jina-embeddings-v2-base-code --embedding-cache target/rsdup-embeddings --format json
rsdup . --mode semantic --embedding-model unclemusclez/jina-embeddings-v2-base-code --similarity-threshold 0.95 --max-body-chars 6000 --view syntax
```

The default `--embedding-backend auto` probes Ollama with a three-second discovery budget. A usable single embedding model is selected without a prompt; missing, unreachable, or ambiguous models lead to a backend choice. Explicit `--embedding-model` implies Ollama and cannot be combined with FastEmbed. Backend failures during a scan abort rather than silently switching models.

**Ollama:** start the service and install your chosen model separately, for example `ollama pull unclemusclez/jina-embeddings-v2-base-code`. rsdup never installs or starts Ollama or pulls its models. Chat-only models are not selected. Ollama requests go only to `http://localhost:11434`, with proxies and redirects disabled. Keep the model tag unchanged during a scan: rsdup checks its digest before and after uncached requests and before publishing the report.

**FastEmbed:** CPU inference uses a pinned, hash-verified revision of `jinaai/jina-embeddings-v2-base-code`. Selecting this backend downloads missing public model/tokenizer files from Hugging Face; embedding inputs are never uploaded. Model files are reused from the OS user cache under `rsdup/fastembed`; set `FASTEMBED_CACHE_DIR` to override this location. Cached model files permit offline inference. Inputs above 8,192 tokens are rejected rather than truncated. No server or external executable is required. The Rust build includes ONNX Runtime, whose build may download a native runtime artifact.

Neither syntax mode loads a model. In our single-pass M1 Max test, uncached CPU embedding took about 160 seconds versus 26 seconds with GPU-backed Ollama; matches were nearly identical, with one borderline pair crossing the default threshold. These are not universal performance guarantees.

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

Semantic JSON has its own schema (`mode: "semantic"`, `schema_version: 1`), leaving syntax JSON version 2 unchanged. It includes extraction/filtering policy, backend-specific model provenance and dimensions, source context, one-based start and exclusive end positions, literal-sensitive identities, input digests, and supporting edges. Edge endpoints and group members index the top-level `units` array. Group IDs depend on retained body identities, paths, kinds, and multiplicity—not source line numbers or similarity scores. Groups are deterministically ordered by source traversal, not by refactoring value.

`--embedding-cache DIR` opts into persistent caching. Entries are keyed by exact input, provider/model identity and digest, and cache-policy version; no source text is stored. Writes are atomic, invalid entries cause actionable errors, and successful requests survive an interrupted scan. Cached vectors can still be sensitive derived data: keep them private. Reports contain original source, so keep reports private too. Neither needs to be committed. Ollama must remain available even on a cache-only run to verify model identity. FastEmbed uses a separate `fastembed-v1` result-cache namespace keyed by verified model artifacts and inference settings; it still loads the local model on cached scans. Model-file caching is automatic, but embedding-result caching remains opt-in for both backends.

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
