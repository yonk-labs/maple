# Evidence — Why (phase 2)

## Bedrock motivation (dug in interview)

**Surface:** subagents run at <32K context; legacy code has god-files (100K+) and
tightly-coupled file clusters whose totality exceeds the window. Whole-file context
doesn't fit; "modernize this file" produces garbage.

**Bedrock (a hard constraint, not a preference):**
> A 32K-context subagent cannot afford to discover its own scope. The scoping must
> happen *outside* its window and arrive *exact and minimal*. Grep has no budget model
> and no stopping condition (the agent burns its 32K on discovery before it writes a
> line). RAG (chunkshop, pg-raggraph) retrieves by *similarity* — fuzzy and lossy — with
> no closure guarantee, and a missed caller is a silent break. A maintained symbol graph
> is the only thing that computes exact blast-radius at near-zero token cost to the agent.

**Why now / why you:**
- **Token-efficiency conviction (personal + company + industry).** 1M+ context windows
  normalize shipping "a lot of crap back and forth." Cheap ≠ free ≠ should. Discipline.
- **Hardware forces the discipline.** 2× NVIDIA DGX Spark boxes (~128GB unified memory
  each), able to run small/mid local models. On local models, context is a *physical
  budget*, not a billing line — this makes byte-sized context existential, not nice-to-have.
- **The pipeline already exists in parts.** Frontier model specs the work; small local
  models do the work. This tool is the context router that makes the small-model tier viable.
- **External ambition.** User believes the pattern is valuable beyond their own pipeline —
  a potential product / OSS tool, not just internal plumbing. (Raises Success to two
  horizons: unblock my pipeline + adoptable by others.)

## Bob — the pipeline this feeds (https://github.com/yonk-labs/bob)

- **What:** autonomous build-verify-judge orchestrator in **Rust** (90%) + Shell. Worker
  counterpart to `abe` (code judge). Runs as MCP server or CLI; Claude Code plugin.
- **Loop:** `task + repo → (isolated git worktree) → BUILD (goose/opencode edits files) →
  scope check (changed files/lines within caps?) → VERIFY (run gate) → JUDGE (abe advises)
  → CONVERGED: apply candidate`.
- **Tiered builders:** cheap (single-shot) / medium-large (`goose` agent loops) / frontier
  (`opencode`). Adaptive model ranking via `.bob/model-stats.json`
  (`success_rate × (1/avg_latency) × 100`).
- **Modules:** engine, builder, verify, judge, worktree, safety (secret-scan + scope caps).
- **Where this tool plugs in:** Bob's BUILD step receives `task + repo`. This tool produces
  the *minimal blast-radius context bundle* per subtask so goose/local-model BUILD fits in
  32K and knows exactly what to touch. Complements Bob's existing scope caps (changed
  files/lines) with *pre-build* scope precision.
- **Stack signal:** Bob is Rust. Strong prior toward a Rust (or Rust-compatible) tool for
  the same ecosystem — revisit in phase 9 (Stack).
