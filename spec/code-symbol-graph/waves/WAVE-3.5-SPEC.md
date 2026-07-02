# WAVE 3.5 SPEC — Greenfield / new-code pathway (T15–T18)

Repo `<repo>`. Prereq: Wave 3 merged, tests green. May touch:
`src/store.rs`, `src/main.rs`, `README.md`. Constraints unchanged (D0, labels, delta==rebuild, K1/K2).

## T15 — Parse-failure visibility (correctness: no silent coverage gaps)
- Store parse failures: new table `parse_failures(file TEXT PRIMARY KEY, error TEXT)`. Populated by
  BOTH `index_repo` and `refresh` (a file that fails to parse is recorded and its old rows removed;
  a file that later parses clean is removed from the table). The file's hash is still recorded in
  `files` so delta doesn't retry it every call unless it changes.
- Surface it: `status` prints `N files unparsed (graph incomplete): <paths…>`; `Bundle.report` gains
  `unparsed_files: Vec<String>` (repo-wide list, capped 10 + count) — an agent consuming a bundle must
  be able to see the graph has holes (spirit of N1). `--format prompt` renders a warning line when
  non-empty. Enumeration gains `unparsed_files_count`.
- Test: fixture with one syntactically-broken .py → indexed repo reports it everywhere above; fixing
  the file + refresh clears it; delta==rebuild equivalence holds across broken→fixed transitions.
  (Note: tree-sitter is error-tolerant — it may produce partial trees instead of failing. If a
  deliberately-broken fixture still parses, use an unreadable/invalid-UTF8 file to trigger the read
  error path, and ALSO record files whose parse succeeded but produced zero defs+calls+imports for a
  non-empty file as `suspect` — document what you could actually trigger.)

## T16 — Duplicate-prevention / module-surface queries
- `maple exists <repo> --name <n>` → JSON `{name, defs:[{fq_name,kind,file,span,signature,docstring}],
  import_names_matches:[{file,source_module}]}` — everything an agent must check before creating a
  new symbol with that name. Empty defs = safe to create.
- `maple surface <repo> --module <path.py|dotted.path>` → JSON `{module, defs:[… module-level +
  classes w/ methods nested …], imports:[raw…], unparsed: bool}` — the API surface an agent extends
  when adding code to that module. Both run `refresh()` first. Deterministic only (N2) — no fuzzy
  name matching (exact name; optionally `--prefix` flag for prefix match, still deterministic).

## T17 — Worktree index seeding
- `maple seed <target-repo> --from <source-repo>`: copy `<source>/.maple/graph.db` into
  `<target>/.maple/` (create dir; refuse if target db already exists unless `--force`), then run
  `refresh()` against the target tree and print the delta stats (changed/deleted/relabeled).
  Result: a bob worktree gets a warm index for O(branch-diff) cost instead of a cold index.
- Test: index fixture repo A; copy tree to B with one file changed; `seed B --from A` → stats show 1
  changed; B's graph == fresh index of B (projection equality, reuse the equivalence helpers).

## T18 — Greenfield lifecycle tests + day-0 docs
- Tests: (a) `index` on a repo with ONE nearly-empty .py → works, status sane; (b) create a new file
  with new symbols → next query's refresh picks them up, `enumerate` sees them, bundle for a
  0-caller symbol returns callers=[] with caller_count=0 (valid, honest — not an error);
  (c) new file calling an old symbol → edge appears; old symbol's closure gains the caller.
- README: add a "Day 0 / new projects" section: `maple index` once at repo creation; every query
  self-heals afterward (no daemon, no hooks); `maple seed` for worktrees; `exists`/`surface` for
  agents writing new code; parse-failure warnings mean the graph has holes.

## Gate
`cargo test` fully green; pg-raggraph edge totals unchanged (read-side + additive features);
smoke: `exists` on a known-duplicated name (`embed`) shows 15 defs; `surface` on
`src/pg_raggraph/config.py` lists its defs; `seed` smoke between two tempdir copies.

## Report back
Tests summary · which T15 failure modes you could actually trigger (parse-fail vs unreadable vs
suspect) · smoke outputs (heads) · deviations · files changed + net lines.
