# Mission Brief — code-symbol-graph (phase 4 evidence)

Framework applied inline (mission-brief) by the phase-engine runtime.

## Purpose
Be the **map** in a local-model coding pipeline: maintain an always-fresh graph of
code symbols so an orchestrator (Hector) can plan minimal slices and each slice's
subagent (Bob → goose/local model, ≤32K context) receives the *smallest complete*
context bundle needed to make a correct change — killing frontier-model escalation
and the "too big, skip it" fallback.

## Ecosystem boundary (who does what)
- **Hector** (Rust TDD/spec planner): decomposes intent into deterministic slices
  (default ≤2 files / 160 lines, frozen scope, verify gate). *Does the partitioning.*
- **code-symbol-graph** (THIS tool): the map — complete closure facts + per-slice
  budget-fit bundles + closure enumeration so Hector can partition. *Not the planner,
  not the worker.*
- **Bob** (Rust build-verify-judge worker): executes a slice in a git worktree.
- **goose / local model**: the ≤32K builder.

## Success Criteria (measurable)
- **SC-1 — Speed.** Warm index: a bundle for a target symbol is produced in **p95 < 1s**
  (delta re-parse of changed files + graph query + assembly). Cold full index is a
  separate one-time cost, bounded in Stack (phase 9).
- **SC-2 — Size (per slice).** A produced bundle is **≤ ~16K tokens** (half of a 32K
  window, leaving room for reasoning + edits; tunable). The tool **reports the bundle's
  token size** so the planner can split further if it doesn't fit.
- **SC-3 — Completeness, no silent gaps.** The graph holds the target's **complete set
  of direct callers + callees**. A bundle for a requested scope contains that scope
  completely, or **explicitly lists what was omitted** — a missing caller is never silent.
  Verified against hand-built fixtures: 100% of direct edges either included or reported.
- **SC-3b — Enumerability.** The tool can answer "symbol X has N callers across M files"
  (count + locations) so Hector can partition a 200-instance change into slices.
- **SC-4 — Sufficiency (the outcome / true-north).** On a benchmark of real subtasks,
  the ≤32K tier completes the slice and passes Bob's VERIFY gate **using only the bundle**
  (zero "need more context" reads/escalations) at a target rate. Baseline TBD — measure
  current frontier-escalation rate first, then set the target.

## Testing requirements
- Fixture repos with known call graphs (incl. a synthetic god-function with N callers)
  to verify SC-3 completeness exactly.
- Token-count assertion on emitted bundles (SC-2).
- Latency benchmark on warm/delta path (SC-1) + cold-index benchmark (Stack).
- End-to-end: feed bundles to the small tier through Bob, measure VERIFY pass rate and
  additional-context requests (SC-4).

## Out of scope (v1)
- Task **planning/partitioning** — Hector's job.
- **Building/editing/verifying** code — Bob's + goose's job.
- Live **daemon/watch** mode — deferred; on-demand delta CLI only.
- Semantic/embedding **similarity retrieval** — this is exact graph closure, not RAG.
- Eager repo-wide **LLM summaries** — summaries are lazy, on-pull, content-hashed (Tier 2).

## Drift checkpoints
- DC-1: Am I building the *map*, or leaking into planning (Hector) / building (Bob)?
- DC-2: Does every feature trace to SC-1..4? (kill gold-plating)
- DC-3: Is "always fresh" still guaranteed for Tier-1 structure with no daemon?
