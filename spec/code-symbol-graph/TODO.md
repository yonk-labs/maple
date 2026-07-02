# maple — consolidated open items (post-v1)

Consolidates every leftover from PLAN.md follow-ups, spike findings, and the accuracy/agent-UX
proposals (2026-07-02). **Protocol:** Fable writes a per-wave spec (mechanism, acceptance checks,
D0/N1-N3 constraints, fixtures) → Sonnet/Opus subagent implements against it → verify on the
pg-raggraph corpus + existing test suite (9 tests must stay green; every resolver change re-runs the
delta==rebuild equivalence test). Waves are ordered so each touches one coherent code area — spec
once, implement together, measure once.

## Wave 1 — Resolver accuracy ✅ DONE 2026-07-02 (spec: waves/WAVE-1-SPEC.md; Sonnet impl, verified)
- [x] T1 type annotations (params + same-file returns; Optional/union/forward-ref simplify)
- [x] T2 `self.attr = ClassName(...)` bindings  ·  [x] T3 import-aware filtering  ·  [x] T4 1-hop inheritance
- **Verified results (pg-raggraph):** exact 3650→**4441**, ambiguous 3001→**2210**, unresolved 7833
  (±0), edges exactly 14484 (D0). **func in-repo 77%→100%** · method 35%→**37%** (target 60% missed —
  honest ceiling: remaining hinted receivers are builtins/external classes (`dict`, `TextEmbedding`,
  `AsyncConnectionPool`); never-guess correctly refuses them; only 6 internal-class-typed params in
  this repo). 18/18 tests green.
- **→ Wave 2 carry-over (new insight):** *known-external receiver reclassification* — when a validated
  hint names a builtin/imported-external class that is NOT in-repo, the true callee is outside the
  repo: label the edge `unresolved(external)` instead of listing false in-repo `ambiguous` candidates.
  Removes ~2.2K noise candidates from bundles; deterministic; needs care (hint must be
  builtin-or-external-import, not a failed-validation in-repo name). This — not more inference — is
  the biggest remaining method-call cleanup on real code.

## Wave 2 — Bundle quality ✅ DONE 2026-07-02 (spec: waves/WAVE-2-SPEC.md; Sonnet impl, verified)
- [x] W2.1 external-receiver reclassification (ambiguous→unresolved for provably-external receivers;
  agent added a `cands>1` guard so it never displaces an exact edge — accepted deviation)
- [x] T5 test-aware bundles (tests first, full test-fn snippets ≤60 lines, cap never evicts tests)
- [x] T6 `--format prompt` · [x] T7 tokenizer seam (`approx`) + `--snippet-radius`
- **Verified:** 22/22 tests; pg-raggraph edges exactly 14484, exact 4441 (±0), ambiguous 2210→**2029**,
  method in-repo exact 37%→**39.4%**; prompt render shows test caller first with 48 assert lines visible.
- *Residual (accepted):* external receivers colliding with exactly one same-named in-repo symbol stay
  "exact" (the guard's trade-off); real per-model tokenizers deferred until a consumer needs them.

## Wave 3 — Agent interfaces ✅ DONE 2026-07-02 (spec: waves/WAVE-3-SPEC.md; Sonnet impl, verified)
- [x] T8 MCP stdio server (`maple mcp` — hand-rolled JSON-RPC, no SDK dep; 3 tools, refresh-first;
  new `src/mcp.rs` + `tests/mcp_spawn.rs` spawn smoke)
- [x] T9 `maple impact --diff <rev>` (hunk-parsed blast radius; deleted-symbols captured pre-refresh;
  documented simplification on pure-deletion hunk semantics)
- [x] T10 FQ-names (fixed a latent bare-name bug in bundle target), docstrings (parser→schema→bundle),
  `module.func` excludes methods, unknown-lang clear error
- **Verified:** 34/34 tests; MCP initialize+enumerate round-trip works; fq_name module-qualified;
  pg-raggraph edges exactly unchanged (14484 / 4441 / 2029 / 8014 — read-side wave confirmed).

## Wave 3.5 — Greenfield / new-code pathway ✅ DONE 2026-07-02 (spec: waves/WAVE-3.5-SPEC.md; verified)
42/42 tests. T15: `suspect` (non-empty file, zero symbols) + `unreadable` (perms) trigger for real —
tree-sitter never hard-fails, documented; surfaced in status/bundle/prompt/enumerate. *Residual nit
(accepted):* docstring-only modules flag as suspect — over-warns, never under-warns. T16 `exists`
(15 embed defs) + `surface` (GLOB, case-sensitive — correct N2 call). T17 `seed` (O(branch-diff)
warm-start, force-guarded, projection-equal to cold index). T18 lifecycle tests + Day-0 README.
Gates: pg-raggraph edges exactly unchanged (14484/4441/2029/8014).
*Original items below for the record:*
- [ ] **T15. Parse-failure visibility (tree health — correctness).** A file that fails to parse is
  currently skipped with a stderr warning → its call-sites are silently absent (violates the spirit
  of N1). Track failed files in the store; surface in `status`/bundle `report` ("2 files unparsed —
  graph incomplete for: …"). Agent-generated half-written files make this common, not rare.
- [ ] **T16. Duplicate-prevention query for new code.** Before an agent creates `def helper()`, ask
  maple: `exists <name>` / module-surface query (`maple surface <module>` → defs + imports of the
  target module = the API an agent extends). Deterministic only (N2) — this is the DRY guard that
  stops agents re-implementing existing helpers.
- [ ] **T17. Worktree index seeding.** Bob worktrees start without `.maple` (gitignored) → cold
  index per worktree. Add `maple seed --from <main-tree>` (copy db, refresh self-heals the branch
  delta = O(diff)). Document the bob campaign pattern.
- [ ] **T18. Greenfield lifecycle tests + docs.** Fixtures: brand-new project (`maple index` on
  near-empty tree; 0-caller bundles are valid honest answers), agent-creates-new-file flow
  (refresh picks up new symbols; enumerate immediately sees them), new-file-that-calls-old-code
  (edges appear both directions). Plus a "day-0 setup" README section: init on repo creation,
  query-time self-heal does the rest — no daemon, no hooks required.

## Wave 4 — Infrastructure hardening ✅ DONE 2026-07-02 (spec: waves/WAVE-4-SPEC.md; verified)
48/48 tests. T11 schema auto-reset via user_version (demoed on the real pre-T11 db — no more raw SQL
errors; queries repopulate via refresh). T12 git-aware O(changes) delta (porcelain candidates +
last_indexed_head, walk fallback; verified live: 1 touched file → 53ms self-healing query). T13 rayon
parallel parse (agent re-benchmarked honestly on this machine: stdlib 10.6s→7.8s, ~27%; SQLite
single-writer phase caps it). T14 Cargo.lock un-ignored, `maple gc --yes`, Operations README.
SC-1 spot-check post-waves: 36ms median warm bundle (git shell-out adds ~20ms vs Wave-1's 13ms; 27×
under target). *Residuals (accepted, ponytail-tagged):* porcelain quoted-path unescaping; stale CLI
about-string (fixed in launch cleanup).
*Original items below for the record:*
- [ ] **T11. Schema versioning/migrations.** `schema_version` table; mismatch → auto re-index instead of
  SQL errors (bit us twice).
- [ ] **T12. Git-aware O(changes) delta detect** (`git diff --name-only` + porcelain; hash walk stays
  fallback). Matters at 100k-file scale only.
- [ ] **T13. Scale pass:** name-index strategy + rayon parallel cold index; benchmark on a 10k+-file corpus.
- [ ] **T14. Repo hygiene:** commit Cargo.lock (binary → reproducible builds); consider `maple gc`.

## Wave 5 — Environment / user-side (not maple code)
- [~] **E1.** ollama server still 0.31.1/`tool_calls: null` (restart pending) — MOOT: local MLX (:8080,
  Qwen3-Coder-Next-4bit) AND DGX vLLM (192.168.1.193:8000, Intel/Qwen3-Coder-Next-int4-AutoRound)
  both return proper structured tool_calls.
- [x] **E2. Bob e2e CONVERGED 2026-07-02:** `bob: CONVERGED in 1 iteration(s)` — maple bundle in task →
  goose → DGX vLLM → exact 1-line fix in worktree → pytest VERIFY pass → advisory judge absent
  (correctly non-blocking per bob's policy code) → propose-mode candidate. **Config recipe discovered:**
  (1) export `OPENAI_HOST=<endpoint>` — goose reads OPENAI_HOST; bob only sets OPENAI_BASE_URL
  (bob one-line fix: set both in GooseBuilder); (2) use bob's roster **Full form**
  (`models: {alias: {model, base_url}}`, tier references the alias) — bare tier strings get
  provider/model-parsed and the id mangled. Every prior `EmptyDiffAfterCritique` traced to these two
  + old-ollama tool parsing — never bob's design, never the models.
- [ ] **E3. SC-4 at scale on DGX:** larger trial set, real task mix, set the formal SC-4 target
  (baseline: bundle 3/3 with qwen2.5-coder vs 2/3 gemma; whole-file 0/2 gemma).

## Deferred (unchanged, K-gated)
Languages beyond Python (universal tier already works; exact resolvers ASK-FIRST K3) · caller
pagination/cursoring (enumerate suffices for Hector) · `--watch` daemon (K1) · lazy LLM summaries (K4)
· depth-N closure (L5) · overflow ranking (L6).

## Wave L1 — Multi-language universal tier ✅ DONE 2026-07-02 (spec: waves/WAVE-L1-SPEC.md; verified)
9 languages (py + rs/c/cpp/cs/java/js/ts/go; TS+TSX grammars). Language-scoped resolution
(symbols.lang, SCHEMA_VERSION 3). 64/64 tests, clippy clean. Python gate byte-EXACT
(14484/4441/2029/8014). Dogfood: maple indexes bob (2167 edges) and ITSELF (mixed rs+py, 3380 edges;
resolve_call closure verified correct). No tree-sitter bump needed (shared ABI shim). Deferred:
per-lang exact resolvers (K3), test-file naming conventions per lang, C/C++ header-decl suspects.
