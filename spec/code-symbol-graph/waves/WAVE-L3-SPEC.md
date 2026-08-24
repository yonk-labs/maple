# WAVE L3 SPEC — Column-level lineage (schema → DML → app code)

Repo `<repo>` = /Users/matt.yonkovit/yonk-tools/maple. `source ~/.cargo/env`. Baseline: all tests
green, clippy -D warnings clean. Constraints unchanged: D0 (every reference site = exactly one edge,
labels exactly `exact|ambiguous|unresolved`), delta==rebuild equivalence, never guess, clippy stays
clean, CI must still pass. Python gate unchanged (pg-raggraph: 14484 edges / 4441 exact / 2029
ambiguous / 8014 unresolved). SQL gate unchanged (Wave L2's dialect fixtures stay green).

Origin: a direct user question — "if I change this column, what's impacted? who else uses it and
for what purpose?" — that L2 (procs/functions/triggers only) cannot answer. `CREATE TABLE` and
`CREATE VIEW` currently produce zero defs, by design (`tsql_ddl_only_is_symbolless_ok`,
`pg_ddl_only_is_symbolless_ok`); DML statement bodies (SELECT/INSERT/UPDATE/DELETE column lists)
are not walked at all today — only procedure/function *call* boundaries are.

## Key design decision — reuse `symbols`/`edges`, don't invent parallel infrastructure

A column is not "callable" the way a function is, but the existing schema already has the right
shape for it:
- **A column def = a `symbols` row**, `kind = "column"`, `parent_class = <table or view name>` —
  exactly the pattern Oracle package members already use (`parent_class = package name`). Table/view
  name itself gets its own `symbols` row too, `kind = "table"` / `"view"`, so `closure`/`enumerate`
  on the table symbol name lists its columns as children the same way a package lists its members.
- **A column reference (read or write) = an `edges` row**, `callee_symbol` = the column's symbol id,
  a new `call_kind` value (`"read"` | `"write"`, alongside the existing `"func"`/`"method"`) instead
  of a new field — `call_kind` is already a free-text column, zero schema change needed there.
- **Payoff:** `closure`, `enumerate`, `bundle`, and — most relevant to the origin question —
  `impact --diff` already walk `symbols`+`edges` generically. A column modeled this way is answered
  by the EXISTING `impact`/`closure` commands almost for free, not by new query surface. This is the
  single biggest scope-reducer in this spec; if a spike (L3.0) finds this doesn't hold, STOP and
  re-plan before writing any walk code — the rest of this spec assumes it does.

No new tables. Two new `symbols.kind` values (`table`/`view` — `column` is a third symbol kind, not
a fourth table). Two new `edges.call_kind` values (`read`/`write`).

## Ship dark — `--sql-columns`, off by default (settled, implemented in L3.1/L3.2)

New, unvalidated extraction behavior does not go on by default the moment the walk code lands.
`maple index --sql-columns` (persisted in the store's meta table, same pattern `--sql-dialect`
already uses) gates table/column def extraction; unset, a repo behaves exactly as it did before
this wave — `CREATE TABLE` parses but contributes zero defs, byte-identical to pre-L3 output. The
gate lives one layer above the walk (`store.rs::parse_one_file`, filtering `kind="table"/"column"`
defs before the suspect-file check runs), not inside the dialect walk functions themselves — the
walk always CAN produce these defs; the store decides whether to keep them. Phase B and Phase C's
new `edges`/resolution output should be gated by the same flag when they land, not a separate one.

## Phase B resolution architecture (settled — see 2026-08-24 Fable review, summarized here)

Neither of the two options originally sketched in Phase B below, cleanly — a synthesis:

- **Dispatch, don't parallel-pass.** maple's incremental refresh (`store.rs`, "pass B") relabels
  edges in *unchanged* files purely by re-running resolution from an edge's stored fields
  (`callee_name`, `call_kind`, `receiver_class`, `call_site_file`, `lang`) — no re-parse. A column
  resolver that isn't reconstructible from exactly that tuple leaves stale labels on DML edges after
  a schema change in a different file. So: a dedicated `resolve_column_ref` function, but dispatched
  from the TOP of the existing `resolve_call` (on `call_kind == "read" | "write"`), not a fully
  separate pass that would have to duplicate pass-B's relabel machinery.
- **Don't share `resolve_call`'s body.** Its candidate query has no `kind` filter and its universal
  fallback (lone same-named candidate → exact) would produce false-exact matches for columns (a bare
  `status` reference matching an unrelated same-named procedure). Dispatch must intercept BEFORE any
  existing branch runs.
- **Scope set rides in `receiver_class`, delimited.** Zero schema change: qualified (`o.status`) →
  `receiver_class = Some("orders")` (single table, alias resolved locally at walk time — same
  mechanism the PL/SQL package-qualifier hint already uses). Unqualified with N candidate tables in
  a FROM/JOIN scope → `receiver_class = Some("orders\x1forder_items")` (join every in-scope table
  name, normalized to match `parent_class`'s casing/quoting). Resolver: `SELECT id FROM symbols
  WHERE name=?col AND lang=?lang AND kind='column' AND parent_class IN (<scope set>)` — 1 row exact,
  2+ ambiguous, 0 unresolved. No fallback tiers, ever.
- **Regression safety is structural, not tested-in.** No existing language emits `call_kind` other
  than `"func"`/`"method"` — a first-line dispatch on `"read"`/`"write"` leaves every current
  resolution path untouched by construction, not just unbroken by the fixture gate.
- **Cross-file ordering is a non-issue.** Cold index inserts every file's symbols before resolving
  any edge — order across files never matters here, confirmed against the actual cold-index loop.
  The incremental refresh trigger (which edges get relabeled after a change) keys off `callee_name`
  among the changed defs, which already covers a column rename/add/drop correctly.
- **One landmine to comment in code when built:** the refresh trigger's `receiver_class=?` half of
  its match won't equality-match a delimited multi-name value — intentional (column-edge relabels
  are always driven by the column NAME changing, not the scope set), but non-obvious enough that a
  future pass could "fix" it by accident without the comment.

## Scope (three phases, deliberately separable — see Sequencing)

- **Phase A — column defs from DDL.** `CREATE TABLE` explicit column list → `column` symbols under a
  `table` parent. `CREATE VIEW` — defer (view columns are usually implicit, derived from the SELECT
  list; needs Phase B's walker to resolve honestly, not a name guess). `ALTER TABLE ADD|DROP COLUMN`
  — recognize and reflect in the *current* state (open question below — see Risks).
- **Phase B — column references inside DML.** SELECT column lists, WHERE/JOIN/ON conditions, INSERT
  column lists, UPDATE SET targets, DELETE WHERE — inside procedure/function/trigger bodies AND
  top-level scripts. Read: SELECT/WHERE/JOIN. Write: INSERT target list, UPDATE SET target,
  `column := expr` (PL/SQL). Table aliases resolve like existing import aliases. An unqualified
  column name resolves via the FROM/JOIN table set: exactly one table defines it → `exact`; 2+ →
  `ambiguous` (never guess which); none of the known tables define it → `unresolved` (could be a
  CTE, temp table, or a table maple hasn't indexed — say so, don't drop it).
- **Phase C — application-code correlation.** ORM-model-aware only, one language/framework first
  (Python: SQLAlchemy declarative models and/or Django models — pick whichever the spike shows a
  cleaner AST shape for). A model field mapped to a db column by explicit convention (`db_column=`,
  SQLAlchemy `Column("real_name", ...)`, or the field name itself when unmapped) is an `exact`
  reference — the mapping is in the code, not a guess. Raw embedded SQL strings
  (`cursor.execute(f"...")`, template-literal SQL) are explicitly OUT OF SCOPE for this wave: string
  interpolation makes the real runtime SQL unknowable at parse time in the general case, and
  guessing here breaks the never-guess contract that's the whole basis of maple's trust model. If
  ever pursued, it's a separate wave, and any signal it produces must be labeled `unresolved`/
  best-effort, never `exact`.

## L3.0 — Spikes (run FIRST, all three gate their phase)

- **S-A (DDL column-list shape):** for each of the 3 dialect grammars already vendored/pinned
  (tree-sitter-sequel-tsql, tree-sitter-postgres, tree-sitter-plsql), does `CREATE TABLE`'s column
  list expose per-column field-named nodes (name, type) the way `CREATE PROCEDURE`'s param list
  already does in L2's walks, or is it an unstructured token list requiring manual token-boundary
  parsing? Test against real DDL (the corpus L2 already dogfooded — Oracle sample schema, pg
  migration repo). If any dialect's grammar can't cleanly expose column boundaries → that dialect's
  Phase A is defs-only-name-list (no type/nullability), documented, not blocked.
- **S-B (DML column-reference shape):** does each grammar expose SELECT/WHERE/JOIN/INSERT/UPDATE
  column references as walkable nodes (e.g. a `column_reference` or `identifier` node distinguishable
  from a function-call identifier by grammar position), or does Phase B need statement-shape
  heuristics per clause type? This is the highest-risk spike in the wave — DML grammars are
  significantly larger surface than the DDL/procedure-boundary grammar L2 already walks. If a
  dialect's DML shape is too irregular to walk exactly, that dialect's Phase B is deferred, not
  guessed at.
- **S-C (ORM AST shape):** spike SQLAlchemy declarative model classes and Django model classes
  against Python's existing S2 resolver — is a `Column(...)`/`models.CharField(...)` class-attribute
  assignment already visible to the existing Python walk (as a def or an assignment target), or does
  Phase C need new Python-specific extraction? Test against a real corpus with both ORMs if available
  (pg-raggraph is SQLAlchemy; find or construct a small Django fixture). Pick ONE ORM to ship first
  based on which spike comes back cleaner; the other stays deferred, not blocked.

## L3.1 — Schema/plumbing
No schema migration beyond the existing `PRAGMA user_version` bump (T11 — any mismatch triggers a
full rebuild, already the mechanism, no new machinery). Add `table`/`view`/`column` to the symbol-kind
vocabulary and `read`/`write` to the call-kind vocabulary; thread through wherever `kind`/`call_kind`
are currently matched exhaustively (JSON output shaping, `closure`/`enumerate` formatting — audit for
assumptions that every symbol is "callable" or every edge is a "call", e.g. bundle's caller-snippet
renderer, which may assume call-site context that doesn't apply to a bare column read).

## L3.2 — Phase A walk (per dialect, gated on S-A)
`CREATE TABLE` column list → `column` symbols, `parent_class` = table name, `symbols` row for the
table itself (`kind = "table"`). Case-folding as established in L2 (SQL identifiers are
case-insensitive; fold at extraction, store stays case-sensitive). `ALTER TABLE ADD COLUMN` — emit
the column as a def at the ALTER statement's location (not the original CREATE TABLE's) — this is
the honest answer to "does maple track schema evolution over time," and it doesn't: the store models
current state, so a table whose columns are assembled from CREATE + multiple ALTERs across possibly
multiple files needs all of them re-indexed together for a complete picture. Document this limit
plainly rather than silently under-representing a table's real column set.

## L3.3 — Phase B walk (per dialect, gated on S-B, biggest lift in this wave)
Walk DML column references inside proc/function/trigger bodies and top-level statements. Alias
resolution (`FROM orders o WHERE o.status = ...` → `o` binds to `orders`) reuses the resolution
pattern already built for import aliases. Builtin/function-call disambiguation: a bare identifier
followed by `(` is a call (existing L2 logic), not a column ref — the two walks must not double-count
the same identifier. Emit `edges` rows with `call_kind = "read"` or `"write"`, resolution label per
the exact/ambiguous/unresolved rule in Scope above.

## L3.4 — VIEW column defs (gated on L3.3 existing)
`CREATE VIEW v AS SELECT a, b FROM t` — view's columns are `t.a`, `t.b` (or their aliases if
`SELECT a AS x`) resolved via L3.3's SELECT-list walker, not re-derived independently. Explicit
column list form (`CREATE VIEW v (x, y) AS ...`) is easier — direct name list, still cross-checked
against the SELECT list's arity as a sanity gate (mismatch → parse_failures entry, not silent).

## L3.5 — Phase C: ORM-aware app-code correlation (gated on S-C, one ORM first)
Model class attribute → column mapping as `exact` edges (`call_kind` — probably a new value,
`"orm-map"` or reuse `read`/`write` if the spike shows model instantiation sites can be distinguished
as reads vs writes; TBD by S-C). Cross-language: the edge's `callee_symbol` points at a `column`
symbol whose `lang` is the SQL dialect, while the `caller_symbol` is a Python `class`/`function` —
confirm `resolve_call`'s existing lang-scoping (added in L2.1, "a T-SQL call never matches a pg def")
doesn't block a deliberate cross-language edge; may need a scoped exception, not a blanket relaxation.

## L3.6 — Tests + fixtures
Per phase: a fixture pair proving `impact --diff` on a changed CREATE TABLE column surfaces every
Phase B reader/writer of that column, across at least 2 files. Ambiguous-unqualified-column test
(two joined tables sharing a column name → `ambiguous`, never guessed). Read-vs-write split test
(one proc reads, another writes, `enumerate` distinguishes them in its counts). ORM test: change a
model field, `impact --diff` surfaces it (if `symbols`/`edges` reuse holds — the design's actual
proof point). Delta==rebuild on a mixed repo including all three phases. Polyglot test: same column
name across two unrelated tables → each resolves independently, no cross-table bleed.

## Gates
- `cargo test` green; clippy -D warnings clean; CI untouched; Python + SQL regression gates EXACT
  (numbers above).
- All existing language/dialect walks byte-identical output (L1 + L2 fixtures untouched and green).
- Dogfood: run `impact --diff` against a real commit that changes one column in a real schema+proc
  corpus (reuse L2's dogfood corpus if it has DML, else source a small one); report what surfaced,
  spot-check by hand that nothing real was missed and nothing false was invented.
- `maple enumerate --symbol <table>.<column>` (or equivalent target-spec form — TBD, may need a new
  spec grammar case since columns aren't "callable" the target-spec grammar currently assumes callable
  symbols) returns real read/write callers with correct resolution labels.
- README: symbol-kind table gains `table`/`view`/`column`; document the Phase C scope boundary
  (ORM-aware only, raw-SQL-string correlation explicitly out of scope) prominently, not buried —
  this is a trust-model boundary, not an implementation detail.

## Report back
Spike verdicts (S-A per dialect, S-B per dialect — this is the wave's real risk, be specific about
which dialects got exact DML walks vs which got deferred, S-C which ORM shipped) · schema-reuse
verdict (did `symbols`/`edges` actually suffice, or did Phase B/C need new tables after all — say so
plainly if the key design decision above didn't hold) · per-phase line counts · all gate outputs ·
judgment calls/deviations · anything deferred (esp. Phase C's raw-SQL-string non-scope — restate it
explicitly here too, this is the thing most likely to get silently expanded under pressure).

## Sequencing note
Phase A is a moderate, well-scoped extension of L2 (S-A spike is low-risk — DDL column lists are
usually grammatically clean; L2 already parsed CREATE TABLE, just discarded the column list). Phase
B is the real unknown (S-B spike result decides feasibility, not just shape) and should not start
until A is solid, since B's fixtures need A's column symbols to resolve against. Phase C should not
start until B is solid for the same reason, AND is a genuinely different kind of engineering
(cross-language correlation, not another SQL walk) — treat it as its own decision point, not an
automatic continuation. Rough estimates, wider error bars than L2's given the DML-walk unknown:
L3.0 ~2-3d (three spikes, S-B is the one that can blow up) · L3.1 ~1d · L3.2 ~3-4d (three dialects) ·
L3.3 ~1-2w (grammar-dominated, per S-B) · L3.4 ~2-3d · L3.5 ~1-2w (first ORM; each additional ORM is
close to a full repeat, not incremental) · L3.6 ~3-4d.

## Open risks (more than L2 had — say so rather than pretend false confidence)
- S-B is the wave's real gate: if DML grammars turn out too irregular to walk exactly across all
  three dialects, Phase B may need to ship dialect-by-dialect (T-SQL first, matching L2's own
  cheapest-first ordering) rather than as one unit — same escape hatch L2.4 (Oracle) used.
- Phase C's cross-language edge is new: every existing edge in the graph today is same-language
  (`resolve_call`'s lang-scoping was built specifically to prevent cross-language false matches in
  L2.1). A deliberate, correct exception here needs care — get this wrong and it's exactly the kind
  of silent-guess bug the whole D0 invariant exists to prevent.
- `ALTER TABLE` sequencing (L3.2) is a real, currently-unsolved gap, not just a documentation note —
  a schema assembled from CREATE + N ALTERs scattered across a migration-file history won't be fully
  represented by indexing any single file. Worth a dedicated follow-up spike if migration-heavy repos
  turn out to be the primary use case (Rails/Django/Alembic-style numbered migration directories).
