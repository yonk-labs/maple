# Constitution — code-symbol-graph (phase 6 evidence)

Governing principles, three-tier framing. Amendments bump intent; log at bottom.

## Identity
A tiny, fast, daemonless CLI that is the **exact, always-fresh map** of code symbols —
the enabling substrate beneath a local-model coding pipeline (Hector plans → this maps →
Bob builds → goose/local model edits). It is the map; never the planner, never the worker.

## NEVER (absolute — no override)
- **N1. Never silently drop a caller or callee.** Every bundle is *complete*, or it
  *explicitly names what it omitted / could not resolve*. A silent miss becomes a silent
  production break when agents refactor in parallel. This is the contract the whole tool
  exists to honor (SC-3).
- **N2. Never guess a call target via fuzzy/similarity/embeddings.** Exact graph
  resolution, or an explicit "unresolved/dynamic" marker. This is not RAG.
- **N3. Never do orchestration or building.** Partitioning/fan-out is Hector's; editing/
  verifying is Bob's. Scope stays "the map." (Drift-checkpoint DC-1.)

## ALWAYS (non-negotiable, no approval needed)
- **A1. Always guarantee fresh *structure* at query time** (delta-validated against file
  hashes/mtimes). Staleness is a correctness bug, not a performance trade (SC-1).
- **A2. Always report ambiguity and unresolved/dynamic calls** in the bundle (the honest
  face of N1: over-inclusion on name collisions is flagged; unresolvable calls are named).
- **A3. Always keep the universal (tree-sitter) tier working for *all* supported
  languages.** Exact per-language resolvers are *additive* behind one seam — never a
  precondition for a language to be usable at the universal tier.
- **A4. Always emit each bundle's token size** so the planner (Hector) can split further (SC-2).

## ASK FIRST (needs explicit approval — guards scope + form factor)
- **K1. A daemon / `--watch` mode.** v1 is an on-demand, delta-driven CLI. A long-running
  process is a *later* optimization, only if the delta check itself is measured as a bottleneck.
- **K2. Heavyweight dependencies** — graph DB (Neo4j), live per-language servers (LSP),
  vector DB. The bet is a tiny binary + embedded store; anything heavier is a deliberate choice.
- **K3. A new exact per-language resolver.** One at a time. *Python is now in v1* (S0 spike found method
  calls 86% ambiguous by name on the user's method-heavy code); resolvers *beyond* Python stay ASK-FIRST.
  Breadth-before-depth is how this decays into a slow, shallow Aider clone.
- **K4. Lazy Tier-2 LLM summaries.** Only if the deterministic tier (signatures, docstrings,
  call-site snippets) proves insufficient in practice. Ship Tier-1 first; earn Tier-2.

## Resolution model (locked in phase 6, mechanism → phase 8 Design)
- **Universal tier** (all tree-sitter languages, day 1): definitions, call-sites, imports;
  **name-based + import-alias-aware** resolution. Over-includes on name collisions (safe for
  N1) and reports it (A2). *Must resolve import aliases* to avoid under-inclusion silent gaps.
- **Exact resolver tier** (per-language plug-in, Python first): scope/import/type-aware
  resolution that removes false positives. Additive; behind one interface.

## Amendment log
- v0.1 (2026-07-01) — initial constitution from spec interview phase 6.
