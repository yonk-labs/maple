# WAVE L2 SPEC — SQL dialects (T-SQL, PostgreSQL, Oracle PL/SQL)

Repo `<repo>` = /Users/matt.yonkovit/yonk-tools/maple. `source ~/.cargo/env`. Baseline: all tests
green, clippy -D warnings clean. Constraints unchanged: D0 (every call-site = exactly one edge,
labels exactly `exact|ambiguous|unresolved`), delta==rebuild equivalence, never guess, clippy stays
clean, CI must still pass. Python gate unchanged (pg-raggraph: 14484 edges / 4441 exact /
2029 ambiguous / 8014 unresolved).

## Scope
UNIVERSAL TIER ONLY for three SQL dialects: defs, call-sites (func vs method kind), name-based
resolution. No receiver hints (nothing is syntactically free in SQL). No imports/aliases (SQL has
none — like C). Concept mapping, all dialects:
- **defs**: `CREATE [OR REPLACE] PROCEDURE|FUNCTION` → `function`; triggers → `function`;
  Oracle `PACKAGE` (spec and body) → `class` container, package members get `parent_class` =
  package name. Signature = first line. Docstring: None everywhere (nothing is cheap).
- **calls**: bare `proc(x)` / `CALL p()` / T-SQL `EXEC p` / plpgsql `PERFORM f()` → kind `func`;
  qualified `pkg.proc()` / `schema.proc()` → kind `method` on the member name (universal split).
  Dynamic SQL (`EXECUTE IMMEDIATE`, `sp_executesql`) → no call-site extracted, honest.
- **case folding**: SQL identifiers are case-insensitive — lowercase every def/call/parent name in
  the SQL walks at extraction. Store stays case-sensitive, untouched. Quoted mixed-case
  identifiers fold too (`ponytail:` note the ceiling; revisit only if a real corpus screams).
- **builtin noise**: calls inside queries (`count`, `sum`, `coalesce`, `now`, `getdate`, …) would
  flood unresolved. Per-dialect skip-list of common builtins (~50 names, one const each), applied
  at extraction (no call-site emitted — D0 intact). Documented in the walk file header.

## L2.0 — Spikes (run FIRST, both cheap, both gate the plan)
- **S-A (pg bodies):** plpgsql function bodies are dollar-quoted strings — does
  `tree-sitter-postgres` (1.2.4, wants tree-sitter ^0.26) parse the body inline, or is it a string
  literal? Test: CREATE FUNCTION with IF/LOOP/PERFORM + a plain `LANGUAGE sql` function. Outcome
  decides L2.3's shape: inline (walk directly) vs two-phase (extract body text, re-parse with the
  same grammar, offset all rows by the body's start line). Also resolves the version matrix:
  workspace tree-sitter is 0.25; check `cargo tree -i tree-sitter-language` per the existing
  Cargo.toml comment — bump workspace tree-sitter only if forced, re-verify all 9 existing grammars.
  If tree-sitter-postgres can't see inside bodies at all AND two-phase re-parse of body text also
  fails to parse plpgsql statements → pg is defs-only; STOP and report before building L2.3.
- **S-B (PL/SQL grammar):** no crates.io grammar exists. Candidate: vendor
  `andreasmaierde/tree-sitter-plsql` (⭐12, frozen Feb 2023 — predates the tree-sitter-language
  shim), regenerate parser with current tree-sitter-cli, build as a path crate under
  `vendor/tree-sitter-plsql/`. Validate against a real corpus (package spec+body, standalone proc,
  trigger, `pkg.proc` calls). If regeneration or parse quality fails → Oracle (L2.4) is deferred to
  its own wave; T-SQL and pg proceed regardless.

## L2.1 — Dialect plumbing (the one design change)
`.sql` can only mean one dialect per repo and sniffing violates never-guess:
- `maple index --sql-dialect=postgres|tsql|plsql`, persisted in the existing meta table so
  refresh/delta inherit it. Unset → `.sql` files are not indexed (today's behavior, backward
  compatible; `maple parse foo.sql` without a dialect errors with a message naming the flag).
- Oracle-only extensions `.pks .pkb .prc .fnc .trg .pls` → plsql unconditionally, no flag needed.
- Registry: `lang_for_path` grows a dialect parameter (threaded from store open — it already owns
  the db handle); the three dialects are three `LangSpec`-shaped entries with lang names
  `sql-postgres`, `sql-tsql`, `sql-plsql`. L1.2 lang-scoped resolution then just works: a T-SQL
  call never matches a pg def.

## L2.2 — T-SQL walk (first: cheapest, proves L2.1)
Grammar: `tree-sitter-sequel-tsql` 0.4.2 (tree-sitter ~0.25 — matches workspace, zero ABI risk).
Bodies are plain statements (no string-body problem). Extract per Scope above; `GO` separators are
just statement boundaries. ~150–250 lines in `langs.rs` (or `langs_sql.rs` if the three walks
together crowd the file).

## L2.3 — PostgreSQL walk (shape decided by S-A)
Grammar per S-A (`tree-sitter-postgres`, else `tree-sitter-sequel` + two-phase). Two-phase rules if
needed: only dollar-quoted or single-quoted bodies of `LANGUAGE plpgsql|sql` functions re-parse;
line offset = body start row; a body that fails to parse yields defs-only for that function
(honest, no partial guessing). `PERFORM f()` → func call.

## L2.4 — Oracle PL/SQL walk (gated on S-B)
Vendored grammar. Package spec members are decls not defs — only package BODY members (and
standalone procs/functions/triggers) emit defs; spec-only entries skipped (mirrors C header
honesty). `pkg.proc()` → method kind; resolution finds the def whose parent_class = pkg via the
existing name-based path. Synonyms/db-links/dynamic SQL → no extraction.

## L2.5 — Tests + fixtures (per dialect, tempdir style like L1.4)
Each dialect: fixture pair proving cross-file `func` call resolves exact; qualified call lands kind
`method`; Oracle adds: package member carries parent_class = package, spec-only decl emits no def.
Case-folding test (`MyProc` def, `MYPROC()` call → exact). Builtin skip test (`count(*)` emits no
edge). Polyglot test extended: same proc name in `.sql` (pg) and `.py` → each caller resolves only
its own language. Delta==rebuild on a mixed repo including `.sql`. Unset-dialect test: `.sql`
ignored, parse errors clearly.

## Gates
- `cargo test` green; clippy -D warnings clean; CI untouched; Python regression gate EXACT (numbers above).
- All 9 existing language walks byte-identical output (L1 fixtures untouched and green).
- Dogfood: index one real proc corpus per shipped dialect (an Oracle sample-schema dump, a pg
  migration repo); report files/symbols/edges + per-kind resolution split; spot-check one closure.
- `maple parse` on each dialect returns symbols; `.sql` with no dialect set errors clearly.
- README: language table gains the SQL rows + a dialect-flag section; document final grammar pins
  (and the vendored PL/SQL provenance/commit if L2.4 ships).

## Report back
Spike verdicts (S-A shape, S-B viability) · final dependency pins / vendored commit · per-dialect
walk line counts · all gate outputs · builtin skip-list contents · judgment calls/deviations ·
anything deferred (esp. if Oracle split off).

## Sequencing note
L2.2/L2.3/L2.4 are independent after L2.0+L2.1 and reorder freely — risk order is T-SQL → pg →
Oracle; if Oracle→pg migration analysis is the business driver, run S-B first and flip. Estimates:
L2.0 ~1d · L2.1 ~1d · L2.2 ~2d · L2.3 ~2–4d (S-A decides) · L2.4 ~1–2w (grammar-dominated).
