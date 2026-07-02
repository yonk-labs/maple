# PLAN — code-symbol-graph

Agent-ready build plan derived from `SPEC.md`. (`superpowers:writing-plans` not installed →
inline task list per the phase-12 degradation path.) Thin vertical slices; the tree compiles and
tests pass between slices. Each task names its acceptance check and the SC/principle it serves.

Stack: **Rust** + **SQLite (`rusqlite`)** + **tree-sitter** + **`rayon`**. Store: `./.maple/graph.db`.

---

## S0 — Validation spike ⛔ KILL-GATE (do this FIRST, throwaway code allowed)
Answers personas P1/P2. Do NOT build the Rust core until this passes.
- [x] **S0.1 — DONE** (`spike/s0_resolution_rates.py`). Measured on pg-raggraph (309 files, 14.5k calls):
  function calls **77% exact** in-repo; method calls **86% ambiguous** in-repo; repo is 53% method calls.
  External bucket clean (builtins/stdlib/3rd-party). **Result: promoted F4 (Python exact resolver) from
  gated fast-follow into v1 (S2).** See `spike/README.md`.
- [ ] **S0.2** Hand-assemble a depth-1 bundle for 5–10 real target symbols; feed to the small (≤32K)
  tier through Bob; compare VERIFY pass rate + "need more context" rate vs. feeding the whole file.
  **Sets the SC-4 baseline + target (currently TBD).** User's environment.
- **GATE:** if S0.2 shows no SC-4 lift even with F4 → rethink (heavier resolution? different target lang?)
  before writing core code.

---

## S1 — Universal tier (v1, first slice) — all tree-sitter languages, over-inclusion + honest flags
One coherent slice, built in compilable sub-slices. Serves SC-1/2/3/3b + Job.
Sufficient *alone* for function-heavy codebases (S0: 77% exact on direct calls).

- [x] **S1.1 Parser (Python) — VERIFIED** (`src/parser.rs`, `cargo test` green). Manual node-walk
  (chose over `tags.scm` to cut version risk); extracts defs (name/kind/span/signature), call-sites
  (func vs method), imports. Real parse of pg-raggraph `__init__.py`: 61 defs, 539 calls, 54 imports;
  `_living_bucket` span verified. Toolchain: rustc 1.96, tree-sitter 0.25.10 + tree-sitter-python 0.23.6.
  *Follow-ups (later in S1):* FQ-name (needs module ctx from S1.2 store), docstring extraction,
  additional languages + unknown-lang error, revisit tags.scm if the manual walk gets unwieldy.
- [x] **S1.2 Store + cold `index` — VERIFIED** (`src/store.rs`, `cargo test` green). SQLite (rusqlite
  bundled, no system dep); schema D1 (files/symbols/imports/edges + name index; edges populated in S1.3).
  Real run on pg-raggraph: **309 files cold-indexed in 0.92s (debug)** → 2244 symbols, 1786 imports, 572K
  db; `status` reload = **0.01s** (~90× faster, proves reload-is-a-read); name-index lookups verified.
  *Cold-index anchor for SC-1:* ~3ms/file debug single-threaded → a 100k-file monorepo ≈ 5min
  single-threaded; release + rayon parallel (S1.6) brings it down — parallelism matters only at that scale.
  *Follow-ups:* store call-sites/edges (S1.3), file hash+mtime already stored for delta (S1.6).
- [x] **S1.3 Resolver — universal — VERIFIED** (`src/parser.rs` enclosing+aliases, `src/store.rs`
  two-phase resolve, `cargo test` green). name+alias resolution; every call-site → exactly one edge
  labeled `exact|ambiguous|unresolved`. Unit test proves: `from a import bar as baz; baz()` binds to
  `bar` (exact, no silent gap); 2-def name → `ambiguous`; `missing()` → `unresolved`.
  **D0 confirmed on real code:** pg-raggraph = 14,484 edges == 14,484 call-sites (nothing dropped).
  **Cross-validates the Python spike exactly** (20.0% exact / 25.9% ambiguous / 54.1% unresolved).
  *Follow-ups:* per-file caller attribution uses first same-named def (rare collision); in-memory
  name index (fine at this scale — revisit for 100k-file monorepos).
- [x] **S1.4 Graph queries — VERIFIED** (`store::closure`/`enumerate`, CLI `closure`/`enumerate`,
  `cargo test` green). Depth-1 closure (direct callers + callees, `--depth` accepted, v1=1); enumeration
  (N callers across M files + per-resolution counts, SC-3b). Real runs: `enumerate embed` → 15 defs / 39
  callers / 25 files / all ambiguous (the planner signal for F4); `closure _living_bucket` → target span,
  5 exact callers w/ file:line, 11 callees (in-repo `_as_aware_utc` exact, builtins unresolved).
  *Follow-ups:* target-spec lookup currently by bare name only (`file:line` / `module.func` forms pending,
  F13); **schema migrations** — column adds don't migrate an existing db (drop `.maple` on schema change;
  add a `schema_version`/migration path before the schema stabilises).
- [x] **S1.5 Bundle + budget + CLI — VERIFIED** (`store::bundle`, CLI `bundle`, `cargo test` green).
  Assembles D4 JSON (target body, callee sigs, caller call-site snippets ±3, `report`); approx tokenizer;
  `over_budget` signal (never trims); fan-in cap (`--max-callers`, default 20) with full count + `omitted`
  reported (N1). Real runs: `bundle _living_bucket` = 1,276-token bundle from a 33K-token god-file
  (96%+ reduction); `bundle embed --max-callers 5` = 39 full fan-in, 5 included, 34 omitted-not-dropped,
  ambiguous_target flagged. Over-budget test: signals but returns complete bundle. Missing symbol → error.
  *Note:* the real `maple bundle` now supersedes the Python `spike/s0_2_bundle.py` harness for S0.2.
  *Follow-ups:* N2 positive-test guard (every edge label ∈ {exact,ambiguous,unresolved}, no score) —
  effectively enforced by the type system (label is set only from the match arms) but add an explicit
  assertion test; pluggable real tokenizer (currently chars/4 approx).
- [x] **S1.6 Delta / incremental — VERIFIED** (`store::refresh`, queries self-heal first,
  `cargo test` green incl. **delta==rebuild equivalence**). Hash-compare detect; re-parse only changed;
  two-pass patch — pass B relabels edges from *unchanged* files (rename case proven: lib.py rename →
  app.py edge relabeled unresolved → new def flips it back to exact). Real pg-raggraph: touch 1 file →
  1 re-parsed, 70 edges relabeled, graph heals to exact canonical counts; query+refresh = **0.03–0.43s**
  (SC-1 < 1s met informally, debug build). *Follow-ups:* git-aware O(changes) detect (hash walk is
  O(files) but cheap — matters only at 100k-file scale); formal p95 benchmark → S1.7.
- [~] **S1.7 Benchmarks — SC-1 ✅ MET (release build).** Warm `bundle` (incl. delta check) on
  pg-raggraph: **p95 = 13ms** (median 12ms, max 380ms first-call) vs the <1s target — 75× headroom.
  Scale: CPython stdlib cold index = **2,254 files / 88K symbols / 405K edges in 3.5s** (~1.5ms/file →
  ~2.5min for a 100k-file monorepo single-threaded; rayon still unneeded); warm enumerate at that scale
  65ms (`namedtuple` → 1 def, 132 callers across 50 files).
  - [ ] **Remaining: SC-4 end-to-end through Bob** (VERIFY pass rate, additional-context requests vs
    whole-file) — needs the user's pipeline; sets the last TBD target. Use `maple bundle` output directly.

---

## S2 — Python exact resolver (v1, second slice) — promoted from gated by S0
Behind the resolver seam (implemented **in-process** in `resolve_call`; out-of-process protocol reserved
for foreign-language resolvers later). Resolvers *beyond* Python remain ASK-FIRST (K3).
- [x] **S2.1 — VERIFIED** (`resolve_call` in `src/store.rs`, parser receiver-class hints,
  `cargo test` green). Three deterministic narrowing rules, never widen/drop (D0): (1) `self.foo()` →
  enclosing class's method; (2) `x = C(); x.foo()` / `C().foo()` → C's method (hint applied only when
  the name uniquely resolves to one class symbol); (3) bare `foo()` → module-level defs only (Python
  scoping can't bind a bare name to a method). Edges store `call_kind` + `receiver_class` so delta
  pass-B re-resolves identically; **S2 delta == rebuild equivalence test green**.
  **Measured on pg-raggraph:** method-call in-repo ambiguity **86% → 65%** (exact 14% → **35%**, 2.5×);
  751 edges narrowed; unresolved + totals unchanged (narrowing-only, D0 held). `embed`: 4 self-calls
  now exact, 35 unknown-receiver correctly still ambiguous.
  *Follow-ups (next narrowing rules, in value order):* parameter **type annotations** (`def f(e: Embedder)`
  → `e.embed()` — syntactic, deterministic, likely the biggest remaining win on typed codebases);
  `self.attr = ClassName(...)` attribute bindings; single-class inheritance walk.

---

## Cross-cutting items — DONE
- [x] **F13 target-spec forms — VERIFIED** (`lookup_targets`, test green): bare `name` |
  `path.py::name` | `path.py:LINE` (innermost containing def) | `module.path.name` (+ `Class.method`
  fallback). Addresses one def among same-named ones (e.g. one of the 15 `embed`s). Bare name stays the
  safe over-set. *Known nit:* `module.func` form doesn't exclude same-file methods (safe over-set).
- [x] **N2 positive guard — VERIFIED** (`n2_label_domain_is_closed` test): every edge label ∈
  {exact, ambiguous, unresolved}; no similarity/confidence scores exist anywhere in the schema.

## Test assets (build alongside S1) — all exist as unit tests in `src/`
- [x] Fixture repos with hand-verified call graphs (tempdir fixtures in `store::tests`).
- [x] Aliased-import fixture (D3 under-inclusion guard). Unknown-receiver fixture (`ambiguous` behavior).
- [x] Token-count + over-budget-signal assertions (SC-2). Delta-vs-rebuild equivalence (D0/A1), incl. S2.

## Definition of done (v1) — status 2026-07-01
- SC-1 ✅ **MET & measured** — warm bundle p95 13ms (release, incl. delta check); stdlib-scale cold index 3.5s.
- SC-2 ✅ **MET** — token_count emitted, `over_budget` signal, fan-in cap + `omitted` report, never trims.
- SC-3 ✅ **MET** — D0 invariant (edges == call-sites on real code), delta==rebuild equivalence green.
- SC-3b ✅ **MET** — enumerate: N callers / M files / per-resolution counts.
- SC-4 ✅ **MEASURED (baseline, 2026-07-01).** Real bug injected into `_living_bucket` (33K-token
  god-file, > the 32K window); VERIFY = repo's own pytest; model = gemma4-32k local via ollama.
  **Maple bundle (1,311 tok): 2/3 pass. Whole file (truncated at 32K): 0/2 pass.** The bundle makes a
  task solvable that whole-file context cannot even represent. Small n — treat as baseline, not final
  target; scale trials on the DGX tier. **Bob e2e plumbing also validated** (bob 0.2.11 + goose 1.39 →
  worktree → 2 iters → honest `EmptyDiffAfterCritique`, applied=false): builder path needs a
  **non-thinking, tool-calling coder model** (thinking models return empty via goose's OpenAI-compat
  path — fix: qwen-coder-class builder or think-off provider config). Log: `spike/README.md`.
- Principles ✅ — N1 (D0 test), N2 (label-domain test), N3 (nothing orchestrates/builds),
  A1 (equivalence + self-heal), A2 (ambiguous/unresolved reporting), A4 (token size). K1–K4 respected
  (no daemon, no heavy deps — SQLite bundled only, Python resolver in v1 per amended K3, no LLM summaries).

**v1 code-complete.** 9/9 tests green. Binary: `target/release/maple`
(`index | status | parse | closure | enumerate | bundle`, JSON on stdout).
