# WAVE 4 SPEC — Infrastructure hardening (T11–T14)

Repo `<repo>`. Prereq: Wave 3.5 merged, tests green. May touch:
`src/store.rs`, `src/main.rs`, `Cargo.toml`, `.gitignore`. Constraints unchanged. `rayon` is already
sanctioned by the original plan for parallel parse (it's in Cargo.toml comments as a later slice) —
adding it in T13 is allowed; no other new deps.

## T11 — Schema versioning (bit us twice)
- `PRAGMA user_version` (or a `meta(schema_version)` table — pick one, document): a `SCHEMA_VERSION:
  i32` const in store.rs. On `Store::open`: empty/new db → set current version; version mismatch →
  **drop all tables and recreate** (auto full re-index happens naturally on next index/refresh — but
  note refresh() must detect "empty files table + non-empty disk" and behave like a cold index; verify
  it does, else make open() print "schema changed; reindex required" and have refresh handle it).
  Never raise raw SQL errors from a stale db again.
- Test: create db, bump a fake older version via PRAGMA, reopen → auto-reset; queries after refresh
  return correct data.

## T12 — Git-aware O(changes) delta detect
- In `refresh()`: if `<root>/.git` exists, get candidate changed paths via
  `git -C root status --porcelain -uall` (covers modified/untracked/deleted vs index+worktree)
  PLUS `git diff --name-only <last_indexed_rev>` when a rev was recorded — simpler acceptable v1:
  porcelain-only against the stored-hash check (porcelain lists what *might* differ from HEAD; a
  commit between maple runs makes porcelain empty while the store is stale, so ALSO store
  `last_indexed_head` (rev-parse HEAD) in a meta table; if HEAD changed, fall back to the full hash
  walk once, then update it).
- Only the listed candidates get hashed/compared (plus the deleted-file check against `files` rows
  restricted to… careful: deletions must still be detected — porcelain shows deletions vs HEAD;
  files deleted AND committed appear via the HEAD-change fallback).
- **Fallback:** non-git repo, git errors, or HEAD changed → existing full hash walk. Correctness
  first: when in doubt, walk.
- Test: git fixture — modify one file without committing → refresh reparses 1 (assert via
  RefreshStats); commit + modify → still correct after HEAD change (falls back to walk once);
  delete a file → detected. Non-git tempdir → walk path still works (existing tests cover).
  Delta==rebuild equivalence must hold in all cases.

## T13 — Scale pass (parallel parse)
- Parallelize the parse step of `index_repo` (and refresh's re-parse loop) with `rayon`: parse files
  in parallel into `ParsedFile`s (pure CPU), then do all SQLite writes single-threaded in the
  existing transaction (rusqlite Connection is not Sync — do NOT share it across threads).
- Benchmark before/after on the CPython stdlib (`python3 -c "import os; print(os.path.dirname(os.__file__))"`,
  ~2.2K files): report cold-index wall clock (was ~3.5s single-threaded release). Delete that
  stdlib `.maple` dir after benchmarking (leave no litter outside the repo).
- Test: existing suite green (equivalence test is the correctness guard); add one test asserting
  index of a multi-file fixture equals the pre-rayon projection (equivalence with refresh already
  covers this — extending it is enough).

## T14 — Repo hygiene
- Remove `Cargo.lock` from `.gitignore` (binary crate → lockfile should be committed).
- `maple gc <repo>`: delete `<repo>/.maple` (with confirmation flag `--yes`); prints what it removed.
- README: brief "Operations" notes (schema auto-reset behavior, gc, seed).

## Gate
`cargo test` fully green · pg-raggraph: `rm -rf .maple` NOT needed anymore after T11 (verify: open a
db written by the pre-T11 binary if available, else simulate via PRAGMA downgrade) · stdlib cold-index
benchmark number reported · pg-raggraph edge stats unchanged from Wave 3.5 acceptance.

## Report back
Tests · benchmark before/after · schema-reset demo output · deviations · files changed + net lines.
