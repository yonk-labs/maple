# WAVE 2 SPEC — Bundle quality + external-receiver reclassification (W2.1–W2.4)

Repo `<repo>` (Rust; `source ~/.cargo/env`). Current state: 18/18 tests
green after Wave 1 (waves/WAVE-1-SPEC.md). Files you may touch: `src/parser.rs` (minimally),
`src/store.rs`, `src/main.rs`. Read all three fully first.

## Constraints (constitution — unchanged)
- **D0:** every call-site = exactly one edge; labels only `exact|ambiguous|unresolved`; total edge
  count on pg-raggraph stays **exactly 14484**.
- **Delta == rebuild:** `refresh` and `index_repo` stay equivalent (same resolve path; extend the
  equivalence test for W2.1).
- Never guess: every rule fires only on deterministically-true preconditions.
- **Note:** unlike Wave 1, W2.1 *intentionally moves edges ambiguous→unresolved*. `exact` must not
  decrease (4441 on pg-raggraph). All other tests stay green (update expectations ONLY where W2.1's
  more-correct semantics changes a fixture outcome — document each).

## W2.1 — External-receiver reclassification (the accuracy carry-over)
**Problem:** a method call whose receiver is *provably* a builtin or external-library class
(`self.pool.get()` where `pool: AsyncConnectionPool` imported from psycopg) currently lists all
same-named in-repo methods as `ambiguous` candidates — pure noise; the true callee is not in the repo.
**Rule:** for a `method` edge with `receiver_class = Some(rc)`, label it **`unresolved`** (external)
iff ALL of:
1. **No in-repo symbol of any kind is named `rc`** (zero rows in `symbols` — if the name exists
   in-repo at all, do nothing; validation ambiguity is not proof of externality), AND
2. `rc` is a **Python builtin type** (hardcode a conservative list: `dict,list,set,frozenset,tuple,
   str,bytes,bytearray,int,float,complex,bool,object,type,BaseException,Exception`) **or** `rc`
   appears in the caller file's `import_names` with a `source_module` that does **not** match any
   in-repo file (reuse the T3 path-suffix matching; no match = external import).
Keep the edge (D0); it just stops polluting the ambiguous candidate list. Where the bundle previously
listed such calls under `ambiguous`, they now appear under `unresolved`.
**Equivalence:** extend the delta==rebuild test: a file whose external import line is edited →
refresh == rebuild. Also: adding an in-repo class named `rc` later must flip the edge back
(pass-B relabel — `rc` lands in the affected-names set via the new def; verify with a test).

## W2.2 — Test-aware bundles (T5)
- A caller is a **test caller** iff its `call_site_file` matches: any path segment `tests` or
  `test`, or basename `test_*.py` / `*_test.py`.
- `BundleCaller` gains `is_test: bool`. Callers sort **tests first**, then by file/line (stable).
- Test callers get a **wider snippet**: the full body of the enclosing test function (look up the
  caller's symbol span via `caller_symbol` → read that span), capped at 60 lines (then fall back to
  the ±3 snippet). Non-test callers keep ±3.
- `report` gains `test_caller_count`. The caller cap (`--max-callers`) counts tests and non-tests
  together, but never evict a test caller in favor of a non-test one.
- Rationale (measured): an SC-4 trial failed precisely because the test's assert lines (expected
  values) were clipped out of the ±3 snippet. Tests are the contract — highest-value tokens.

## W2.3 — `--format prompt` (T6)
`maple bundle … --format prompt` (default remains `json`) prints task-ready markdown to stdout:
```
# Target: <fq_name> (<file>:<start>-<end>)
```python
<target body>
```
## Direct callees
- `<signature>`  (<file>)          [one line each; unresolved/external noted]
## Callers (N total; M shown; tests first)
### <file>:<line> in <caller>()   [TEST]
```python
<snippet>
```
## Report
tokens≈<n> budget=<b> over_budget=<bool>; omitted: <k> callers (<file:line …>); ambiguous: […]; unresolved: […]
```
Exact layout may vary; requirements: fenced code blocks for all code, tests flagged, omissions and
ambiguity/unresolved ALWAYS rendered (N1 — the report is not optional), token count included.
Implement as a rendering of the existing `Bundle` struct — no separate assembly path.

## W2.4 — Tokenizer seam + snippet tuning flags (T7, minimal)
- Introduce a `Tokenizer` trait (`fn count(&self, s:&str)->usize`) with the current chars/4
  approximation as the only impl (`approx`). Wire `--tokenizer approx` flag (single valid value for
  now) so future per-model counters slot in without CLI changes. **No new dependencies** (K2).
- Add `--snippet-radius <n>` (default 3) to `bundle`. `meta.tokenizer` reports the tokenizer name.

## Tests required
- W2.1: fixture with `from external_sdk import Pool; class C: def __init__(self): self.p = Pool()` +
  `self.p.get()` + an in-repo `def get` elsewhere → edge is `unresolved` (was ambiguous). Counter-case:
  receiver hint naming an in-repo function (not class) → unchanged. Builtin case: `x = dict(); x.get()`
  → unresolved (wait — `dict()` ctor-binding gives receiver_class "dict"; with an in-repo `def get`
  it was ambiguous; now unresolved). Flip-back case: add class named like the external → pass-B relabels.
- W2.2: bundle over a fixture where a test file calls the target → caller flagged `is_test`, listed
  first, snippet contains the full test fn incl. an assert line beyond ±3; non-test snippet stays ±3;
  cap never evicts the test caller.
- W2.3: `--format prompt` output contains the target fence, `[TEST]` flag, report line with token
  count and omissions (assert on substrings, not exact layout).
- W2.4: radius flag honored; tokenizer name in meta.
- Equivalence + N2 domain tests stay green.

## Measurement gate (report before/after)
Same pg-raggraph edge-stats command as Wave 1 (delete `.maple` first — schema may change only if you
add columns; prefer no schema change: `is_test` is computed at bundle time, not stored).
Expected: total 14484; exact ≥ 4441 (unchanged unless a fixture-logic fix); **method `ambiguous`
drops substantially** (analysis says ~200+ hinted-external edges plus builtin-receiver cases move to
unresolved); report the method in-repo exact% (numerator unchanged, denominator shrinks — expect a
jump past 37%). Also run:
`./target/release/maple bundle ../pg-raggraph --symbol "src/pg_raggraph/__init__.py::_living_bucket" --format prompt | head -40`
and confirm the test caller (`tests/unit/test_living_knowledge.py`) renders first with visible asserts.

## Report back
Tests (all green, count) · before/after edge stats + method in-repo % · the `--format prompt` head
output · any spec deviation and why · files changed + net lines.
