# WAVE 1 SPEC — Resolver accuracy (T1–T4)

Implement four deterministic narrowing rules in maple's Python resolver. Repo:
`<repo>` (Rust; `source ~/.cargo/env`; `cargo test` must stay green).
Files touched: `src/parser.rs`, `src/store.rs` only. Read both fully before editing.

## Non-negotiable constraints (constitution)
- **D0:** resolution never drops a call-site; every call-site = exactly one edge labeled
  `exact | ambiguous | unresolved`. Rules may only **narrow** (ambiguous→exact) or keep; on any
  uncertainty, fall back to the previous (broader) behavior. Never guess: a rule applies only when
  its precondition is deterministically true.
- **Labels:** only those three strings. No scores (N2).
- **Delta == rebuild:** `store::refresh` and `store::index_repo` must produce identical graphs
  (existing equivalence test, which you will extend). Both paths must use the same resolve function.
- All existing tests stay green **except the two expectation updates in §T3** (explained there —
  they encode *more correct* Python semantics, not a regression).

## Current architecture (orientation)
- `parser.rs`: tree-sitter walk → `Definition{name,kind,parent_class,start_line,end_line,signature}`,
  `CallSite{name,kind:"func"|"method",line,enclosing,receiver_class:Option<String>}`, `Import{raw,line}`,
  `Alias{local,source}`. A `Ctx` carries `enclosing_fn`, `method_class`, `self_class`. Ctor bindings
  (`x = ClassName(...)`) are collected per-function and a post-pass sets `receiver_class` when a var
  binds to exactly one class name. `self.foo()` → `receiver_class = self_class`. `C().foo()` → `C`.
- `store.rs`: SQLite `files/symbols/imports/edges`. `resolve_call(conn,name,call_kind,receiver_class)`
  filters candidates: method+validated-receiver-class → `parent_class == rc`; func → `parent_class IS
  NULL`; empty filter → fallback to full candidate set. `index_repo` (two-phase) and `refresh`
  (delta + pass-B relabel of edges `WHERE callee_name=?1 OR receiver_class=?1` per affected name) both
  call it. Receiver hints are **validated at resolve time** (hint must uniquely name one class symbol).

## T1 — Type-annotation resolution (parser + tiny schema)
1. **Param annotations.** In `function_definition` parameters, handle `typed_parameter`
   (and `typed_default_parameter`): param `x: T` becomes a **binding** `(fn_name, x, simplify(T))` —
   reuse the existing ctor-binding post-pass so `x.foo()` gets `receiver_class = simplify(T)`.
   `simplify(T)`: plain `identifier` → itself; `Optional[X]` / `X | None` / `None | X` where X is a
   plain identifier → `X`; string forward-ref `"X"` → `X`; **anything else (generics like `list[X]`,
   unions of 2+ real types, attributes like `a.B`) → no binding.**
2. **Return annotations, same-file only.** `Definition` gains `ret_class: Option<String>` =
   `simplify(return annotation)`. Post-pass in `parse_python`: any ctor-style binding
   `(fn, x, Name)` where `Name` matches a **same-file** function def with `ret_class=Some(R)` is
   rewritten to `(fn, x, R)`. (Cross-file return typing is out of scope — deterministic same-file only.)
3. Existing resolve-time validation (hint must uniquely name one class) stays and covers both.
- Schema: `symbols` gains nullable `ret_class TEXT` (persist it; used only by parser post-pass now,
  but store it for later waves). Bump nothing else.

## T2 — `self.attr = ClassName(...)` attribute bindings (parser only)
- During the walk, when inside class C (i.e. `self_class=Some(C)`), an assignment
  `self.attr = ClassName(...)` records a **class-scoped binding** `(C, attr, ClassName)`.
- Method calls whose receiver is `self.<attr>` — AST: `call → function: attribute(attribute: name,
  object: attribute(object: identifier "self", attribute: attr))` — get
  `receiver_class = ClassName` **iff** attr binds to exactly one distinct class name across all of
  C's methods; two+ distinct → no hint.
- Same resolve-time validation applies. No schema change.

## T3 — Import-aware candidate filtering for `func` calls (parser + store + resolve signature)
1. **Parser:** emit structured import names in addition to raw imports. New `ParsedFile.import_names:
   Vec<ImportName{local:String, source_module:String}>`:
   `from a.b import c` → `{c, "a.b"}`; `from a.b import c as d` → `{d, "a.b"}` (keep the existing
   `Alias` emission too — it feeds name expansion); `import a.b` → `{a, "a"}`-style module imports may
   be skipped (module-attribute calls are `method` kind; out of scope). Skip `from x import *`.
2. **Store:** new table `import_names(file TEXT, local TEXT, source_module TEXT)`; populated on index
   and refresh (delete+reinsert per changed file like `imports`).
3. **Resolution for `call_kind=="func"`, name N, in caller file F** (add `call_site_file` param to
   `resolve_call`; edges already store it — pass it in `index_repo`, `refresh` edge-insert, AND pass-B
   relabel):
   - Build candidate set = all module-level defs (`parent_class IS NULL`) named N (post-alias name).
   - **(a)** If `import_names(F)` has N with source_module M → prefer candidates in files matching M
     (path suffix `M.replace('.','/') + ".py"`, matched as `file = ? OR file LIKE '%/' || ?`), **plus**
     any same-file (F) def of N. 1 → exact; ≥2 → ambiguous; 0 → step (b).
   - **(b)** Else if F itself has a module-level def of N → exact to it (Python module scoping).
     (Multiple same-file defs of N: last one wins in Python; picking the last by line is acceptable,
     or mark ambiguous — choose one, document in code.)
   - **(c)** Else fall back to current behavior (all module-level, then full set).
4. **⚠ Required test-expectation updates** (more-correct semantics, update assertions + comments):
   - `resolves_edges_and_upholds_d0`: `dup()` called in `b.py`, which *defines its own* `dup` → now
     binds **exact** to b.py's dup (was `ambiguous`). Expect `exact=3, ambiguous=0, unresolved=1`,
     still 4 edges total (D0 intact).
   - `closure_and_enumerate`: `enumerate("dup")` → `ambiguous=0`, exact count 1 for that caller.
   - Do NOT weaken the resolver to preserve old expectations.

## T4 — Single-hop inheritance walk (parser + schema + resolve)
- Parser: `class C(Base):` → `Definition.base_class = Some("Base")` when the superclass list contains
  **exactly one plain identifier** (skip qualified names, generics, multiple bases, metaclass kwargs).
  Schema: `symbols` gains nullable `base_class TEXT`.
- Resolve, method path: receiver class rc validated & has **no own** method N → if rc's class def has
  `base_class=Some(B)` and B uniquely names one class symbol → look for B's own method N; exactly 1 →
  exact. Anything else (no base, ambiguous B, B lacks N) → existing fallback. **One hop only.**

## Schema note
No migrations exist yet (Wave 4). Tests use fresh temp dirs. For any manual run against an existing
`.maple/graph.db`, delete the `.maple` dir first. Mention this in your final report.

## New tests required (add to existing suites; use tempdir fixtures like current tests)
1. T1: two classes A,B both `def foo`; `def use(a: A): a.foo()` → exact→A.foo. Variants: `Optional[A]`,
   `A | None`, `"A"` → exact; `list[A]` and unannotated param → still ambiguous.
2. T1 ret: `def make() -> A: ...` ; `def use(): x = make(); x.foo()` → exact A.foo (same file).
3. T2: `class C: def __init__(self): self.p = A()` + `def m(self): self.p.foo()` → exact A.foo;
   attr rebound to A and B in different methods → stays ambiguous.
4. T3: modules m1.py/m2.py each `def dup`; caller `from m1 import dup; dup()` → exact m1.dup;
   caller with own `def dup` → exact same-file; caller with neither import nor local def → ambiguous.
5. T4: `class Base: def foo` ; `class C(Base): pass` ; `x = C(); x.foo()` → exact Base.foo;
   two-hop chain or `class C(B1, B2)` → not narrowed.
6. **Extend the delta==rebuild equivalence test:** after (i) editing an import line in one file and
   (ii) renaming a base class, refresh() equals a fresh index_repo() on the same tree (id-free
   projection compare, as the existing test does). Pass-B hint: base-class rename appears in the
   affected-names set (class def name changed), and `receiver_class IN affected` already triggers
   relabel; verify it actually catches the T4 path.
7. N2 domain test must still pass unchanged.

## Measurement gate (run after all tests green; include in final report)
```bash
source ~/.cargo/env && cargo build --release
rm -rf ../pg-raggraph/.maple
./target/release/maple index ../pg-raggraph
python3 -c "
import sqlite3
c=sqlite3.connect('../pg-raggraph/.maple/graph.db')
for ck in ('func','method'):
    rows=dict(c.execute(\"SELECT kind,COUNT(*) FROM edges WHERE call_kind=? GROUP BY kind\",(ck,)).fetchall())
    tot=sum(rows.values()); inr=rows.get('exact',0)+rows.get('ambiguous',0)
    print(ck, rows, '| in-repo exact %.0f%%' % (100*rows.get('exact',0)/inr if inr else 0))"
```
**Baseline (must improve):** total exact 3650 / ambiguous 3001 / unresolved 7833; func in-repo 77%
exact; method in-repo 35% exact. **Target:** method in-repo exact ≥ 60% (stretch ≥ 77%); func ≥ 85%;
unresolved total unchanged ±0 (narrowing rules must not touch unresolved counts); **edge total exactly
14484** (D0).

## Report back (final message)
Tests summary (all green, incl. updated expectations) · the measurement table before/after ·
any rule you had to weaken/skip and why · files changed with line counts.
