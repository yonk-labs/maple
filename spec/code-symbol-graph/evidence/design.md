# Design evidence — code-symbol-graph (phase 8)

## Grounded library facts
- **tree-sitter tags** provides `@definition.function`, `@definition.method`, `@reference.call`.
  Captures are **syntactic only — no cross-file resolution** (confirmed:
  https://tree-sitter.github.io/tree-sitter/4-code-navigation.html). So def/call-site extraction
  is off-the-shelf; name→definition resolution (alias-aware) is *our* layer. This is the moat line.
- Delta pattern (mtime/hash + on-disk cache) is Aider's proven `repomap.py` approach.
- git-aware change detection: `git diff --name-only <rev>` + porcelain working-tree status gives
  O(changes) instead of O(files) for the freshness check when the target is a git repo.

## Key invariant (the whole correctness story)
**Resolution never deletes a call-site; it only labels it.** A call-site captured by
`@reference.call` becomes an edge that is always one of: `exact` (one callee), `ambiguous`
(candidate set, all kept), or `unresolved` (dynamic/reflection/missing target). This is what makes
N1 (no silent drop) a structural property, not a hope: the only way to lose a caller is to fail to
parse a file or discard a capture — both forbidden and both testable (coverage + equivalence tests).

## Open questions carried to Stack/Architecture
- Exact tokenizer strategy per target model (see Token budgeting block in SPEC).
- Concurrency model for parallel re-parse (rayon-style pool) — Stack phase.
- Whether the resolver seam is a trait (in-process) or a subprocess protocol (out-of-process per-lang).
