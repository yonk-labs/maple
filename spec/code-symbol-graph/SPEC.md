---
slug: code-symbol-graph
input: "A tiny, fast tool/service that keeps an always-up-to-date graph of code symbols and functions (AST/tree-split), so subagents (<32K context) can be handed precise, byte-sized, targeted context — a specific function plus its downstream callers and enough summary to understand blast radius — instead of whole 100K+ files. Hyper-focused code-graph; alternatives like chunkshop and pg-raggraph may be too slow to keep fresh."
intensity: standard
updated: 2026-07-01
plan: PLAN.md
phases:
  frame: confirmed
  why: confirmed
  users: confirmed
  success: confirmed
  compete: confirmed
  vision: confirmed
  features: confirmed
  design: confirmed
  stack: confirmed
  architecture: confirmed
  personas: confirmed
  synthesize: confirmed
---

# SPEC — code-symbol-graph

> The authoritative spec. A coding agent should be able to build from this file
> alone; `evidence/` holds the supporting research and consults.

## Overview / Current State
<!-- phase: frame -->
**Premise (one line):** A tiny, fast indexer that maintains an always-fresh graph of code symbols (functions/methods) via AST parsing, so that when a task targets a symbol it can hand a subagent a byte-sized context bundle — the target function, its downstream callees, its upstream callers, and enough summary to understand blast radius — instead of a whole 100K+ file.

**Intake mode:** Idea mode (greenfield; no existing codebase to reverse-engineer).

**Product shape (locked in frame):**
- **On-demand CLI backed by a persistent, delta-driven store.** No daemon. The orchestrator invokes it right before dispatching a subagent; the tool stats/hashes files, re-parses only what changed since last run, patches the graph, then answers. Cold full index is paid once; every subsequent call is a millisecond-scale delta. A `--watch` daemon mode is explicitly a *later* optimization, not v1.
- **Two data tiers with opposite cost profiles:**
  - **Tier 1 — structural facts (deterministic, delta-cheap, always exact):** definitions, signatures, docstrings, call edges (callees + callers), imports, and **call-site snippets** (the real invocation + 2–3 lines of context). This is the "always up to date" tier with no asterisk. Call-site snippets answer most of "how is this used?" deterministically, without an LLM.
  - **Tier 2 — semantic summaries (LLM, expensive, delta-hostile):** a short prose "what/why" gloss. Never eagerly maintained repo-wide. Generated *lazily on pull* and cached keyed by the symbol's content hash; regenerated only for the one symbol whose hash changed. The fast Tier-1 core never blocks on it.
- **Freshness guarantee:** structure is *guaranteed-fresh*; summaries are *fresh-on-pull*.

**Parked for Design (phase 8):** exact Tier-2 mechanism — call-site snippets only, or + lazy LLM gloss; how many lines of call-site context; how "blast radius" is scoped and depth-bounded.

## Problem & Why
<!-- phase: why -->
**The problem, precisely.** A subagent (e.g. `goose` on a local model, ~32K max context for resource reasons) is handed coding subtasks by a task-splitter. Legacy apps have god-files (100K+ lines) and tightly-coupled file clusters whose totality exceeds the window. To fix something surgically you must know *exactly which functions to change and what the blast radius is* — with the smallest possible surface area.

**Why a maintained graph, not grep or RAG (the bedrock).**
> A 32K-context subagent cannot afford to discover its own scope. The scoping must happen *outside* its window and arrive *exact and minimal*. Grep finds things but has no budget model and no stopping condition — the agent burns its 32K on discovery before writing a line, and on a god-file the discovery alone overflows. RAG retrieves by *similarity* (fuzzy, lossy) with no closure guarantee — a missed caller is a silent break. A maintained symbol graph is the only thing that computes exact blast-radius at near-zero token cost to the agent.

**Why now / why you.**
- **Token-efficiency conviction.** 1M+ context windows normalize shipping crap back and forth. Cheap ≠ free ≠ should. This is a discipline the user holds personally, for the company, and sees as an industry problem.
- **Hardware forces it.** 2× NVIDIA DGX Spark boxes (~128GB unified memory), running small/mid local models. On local models context is a *physical budget*, not a billing line — byte-sized context is existential, not nice-to-have.
- **The pipeline exists in parts** — [`yonk-labs/bob`](https://github.com/yonk-labs/bob): a Rust build-verify-judge orchestrator that BUILDs via `goose`/`opencode` in isolated worktrees with scope caps. This tool is the **pre-build context router** that feeds Bob's BUILD step a minimal blast-radius bundle so the small-model tier can actually do the work. Frontier model specs; small models execute.
- **External ambition.** The user believes this pattern is valuable *beyond* their own pipeline — a candidate product/OSS tool, not just internal plumbing. → Two success horizons (see Success Criteria): unblock my pipeline **and** be adoptable by others.

_Evidence: `evidence/why.md` (bedrock derivation + Bob summary)._

## Users & Use Cases
<!-- phase: users -->
**Primary user (v1): the pipeline operator** — i.e. the person running the Bob + goose + local-model pipeline on their own hardware. Has the itch, the DGX boxes, and the ground truth to know when it works. External developers/teams are **user #2**, served *not* by building for them now but by keeping v1 a clean standalone CLI with no yonk-only assumptions.

**The job (hired for):**
> "Given a coding subtask that names a target, hand my subagent the *smallest complete* context bundle it needs — the target function, what it calls, what calls it, and the blast radius — so a 32K local model can correctly make a change that today either fails or has to be kicked up to a frontier model."

**What they do today (dominant fallbacks — what this must beat):**
1. **Escalate to the frontier model.** Works, but it *is* the token-waste this project exists to kill.
2. **Leave the legacy code untouched.** The small-model tier never attempts god-files, so the work doesn't happen.

_(Lesser/secondary fallbacks — feeding whole files, letting goose grep-and-flail — exist but are not the dominant behavior.)_

**Success therefore = the operator trusts the small-model tier enough to stop escalating and stop skipping.**

## Success Criteria
<!-- phase: success -->
**The tool is the *map*, not the planner and not the worker.** Ecosystem:
`Hector (plans slices) → code-symbol-graph (this: the map) → Bob (build-verify-judge) → goose/local model`.
Because Hector partitions a large change into slices (default ≤2 files / 160 lines), this tool
never crams a whole god-function's callers into one bundle — it must *know all of them* so Hector
can split, and emit a *budget-fit bundle per slice*.

- **SC-1 — Speed.** Warm index: a bundle for a target symbol is produced in **p95 < 1s** (delta re-parse of changed files + graph query + assembly). Cold full index is a separate one-time cost, bounded in Stack.
- **SC-2 — Size (per slice).** A bundle is **≤ ~16K tokens** (half the 32K window — leaves room to reason + edit; tunable). The tool **reports each bundle's token size** so Hector can split further if needed.
- **SC-3 — Completeness, no silent gaps.** The graph holds the target's **complete set of direct callers + callees**. A bundle for a requested scope contains it completely, or **explicitly lists what was omitted** — a missing caller is never silent. Verified against hand-built fixtures: 100% of direct edges included-or-reported.
- **SC-3b — Enumerability.** The tool answers "symbol X has N callers across M files" (count + locations) so Hector can partition a 200-instance change into slices.
- **SC-4 — Sufficiency (true-north outcome).** On a benchmark of real subtasks, the ≤32K tier completes the slice and passes Bob's VERIFY gate **using only the bundle** (zero "need more context" escalations) at a target rate. Baseline TBD — measure current frontier-escalation rate first, then set the target. SC-1/2/3 are the levers that make SC-4 possible.

_Evidence: `evidence/mission-brief.md` (Purpose, testing requirements, out-of-scope, drift checkpoints)._

## Competitive Landscape
<!-- phase: compete -->
The market splits three ways and leaves this tool's exact quadrant empty. (Full table + sources: `evidence/market-intel.md`.)

- **Exact-closure tools — built for humans or heavy/lagging, no budget bundle:** Sourcegraph SCIP/LSIF, Glean, Kythe, LSP call hierarchy, Blar/blarify, `stack-graphs`. Precise, but none emit a token-budgeted bundle; the fast ones need a live server, the thorough ones lag.
- **Bundle/budget tools — approximate, no closure guarantee:** **Aider repo map** (the closest overall — tree-sitter + PageRank, but *ranks and truncates*, drops low-rank symbols to fit; cannot guarantee the complete caller set), code2prompt, repomix, gitingest (whole-repo dumps + token count).
- **RAG/similarity tools — closure isn't even a goal, misses are silent by design:** Cursor indexing (docs explicitly: no caller/callee graphs), Continue `@codebase`, and the user's own **chunkshop**.

**Closest 5:** (1) Aider repo map — same primitives, *inverse* design; (2) Blar/blarify — exact graph for agents but Neo4j-heavy, incremental+budgeting are future work; (3) **pg-raggraph (user's own)** — already does exact AST closure but its graph is *out-of-band and lags the corpus* — that freshness gap is this tool's entire reason to exist; (4) headless LSP call hierarchy — exact but per-language-server, no bundling; (5) Potpie — graph+agent but RAG-flavored, Neo4j-heavy.

_Note: chunkshop and pg-raggraph are the user's own yonk-labs repos — self-comparables / prior pipeline parts, not external competitors._

## Differentiation
<!-- phase: compete -->
**The empty quadrant = all four guarantees at once**, which no surveyed tool ships together:
1. **Exact caller/callee closure for a targeted symbol, with explicit no-silent-drop / omission reporting** (SC-3).
2. **Enumerability** — "X has N callers across M files" as a first-class answer a planner (Hector) partitions on (SC-3b).
3. **Select-to-fit token budget** — assembles to fit, doesn't dump-and-count (SC-2).
4. **Guaranteed-fresh via cheap delta re-parse, no daemon** (SC-1).

**This tool is not the moat — it's the enabling substrate of the moat.** The pipeline's defensibility is the harness (Hector's parallel slicing + Bob's verify loop). That harness is only *safe* if the map underneath is exact (no dropped caller → no silent break when 8 agents refactor in parallel), fresh (each parallel session sees current truth), and budget-fit (every slice fits 32K). A fuzzy or stale map makes parallel refactoring dangerous. This tool earns its place by being the component with the tightest contract — orchestration stays out of its scope (drift-checkpoint DC-1).

**Survives the "weekend clone" counter-argument:** a demo, yes; the real thing, no. Every *primitive* is off-the-shelf (tree-sitter, tags, tiktoken, mtime cache — Aider proves you can wire an approximate map fast). But **no off-the-shelf library provides incremental, cross-file, *exact* caller→callee resolution across multiple languages through one interface.** `stack-graphs` is closest yet is Python/JS/TS-only, resolves def↔ref (not call edges), and was archived Sept 2025; per-language call-graph tools (PyCG, go/callgraph, java-callgraph, Jelly) are heavy, single-language, and share no interface. Normalizing that into a fast, fresh, exact layer *is* the product — a multi-week integration, not a weekend.

**Honest moat statement:** integration + guarantee + form-factor bet, **not IP**. Defensibility is speed-of-execution and a narrow, verifiable correctness contract — not a secret algorithm. Blar could add budgeting; LSP-drivers could add bundling. The bet is being the one tool that runs exact-closure before *every* dispatch, fast and daemonless.

**⚠ Two tensions carried forward:** exactness (SC-3) vs. fast-daemonless (SC-1) is real — exactness usually costs a heavy semantic pass or a lagging out-of-band build; landing both is the hard part (→ Design/Stack). And exact cross-file resolution doesn't come as a multi-language lib — **resolved in Vision/Features:** v1 ships the *universal tier* for all languages (name+alias resolution: over-includes safely + honest flags), and *exact* per-language resolvers are a gated, additive fast-follow behind one seam (Python first). No single-language cut — breadth at the universal tier, depth per-language over time.

## Product Vision & Principles
<!-- phase: vision -->
**The feel:** a Unix-y, composable CLI. Call it with a target (`maple bundle --symbol foo.bar --budget 16k --format json`); it returns a compact JSON bundle on stdout in well under a second, and you *trust it completely* — if it says "these are the callers," that's all of them, or it names what it left out. No server to babysit, no config ceremony, no chat loop. Composes into Hector/Bob like `jq` in a shell pipeline. Tiny binary, obvious contract, boring to operate.

**Resolution model (locked here; mechanism → Design):** two tiers.
- **Universal tier** — all tree-sitter languages, day one: definitions, call-sites, imports. Resolution is **name-based + import-alias-aware**; it *over*-includes on name collisions (safe — never drops a real caller) and **reports** the ambiguity. Must resolve import aliases to avoid under-inclusion.
- **Exact resolver tier** — a per-language plug-in seam (Python first): scope/import/type-aware resolution that removes false positives. Additive; no core rebuild. Satisfies "Python easiest, but all will be needed."

**Non-negotiables (constitution — `evidence/constitution.md`):**

*NEVER:*
- **N1 — Never silently drop a caller/callee.** Complete, or explicitly name what was omitted/unresolved. The core contract (SC-3).
- **N2 — Never guess a call target via fuzzy/similarity.** Exact resolution or an explicit "unresolved/dynamic" marker. Not RAG.
- **N3 — Never do orchestration (Hector) or building (Bob).** Stays the map (DC-1).

*ALWAYS:*
- **A1 — Fresh structure at query time** (delta-validated). Staleness is a correctness bug (SC-1).
- **A2 — Report ambiguity + unresolved/dynamic calls** (the honest face of N1).
- **A3 — Universal tier works for all languages**; exact resolvers are additive behind one seam.
- **A4 — Emit each bundle's token size** so Hector can split (SC-2).

*ASK FIRST:*
- **K1 — daemon/`--watch` mode** (v1 is on-demand delta CLI); **K2 — heavyweight deps** (graph DB, live LSP, vector DB); **K3 — a new per-language exact resolver** (one at a time, Python first); **K4 — lazy Tier-2 LLM summaries** (earn them; ship deterministic Tier-1 first).

## Features & Requirements
<!-- phase: features -->
_Critical = at least one Success Criterion or Job fails without it. Derived backward from Jobs + SC-1..4. `→ Design` = mechanism worked in phase 8. Slice = v1 build order (thin vertical slices; S1 is independently shippable & testable)._

| ID | Feature | How (sketch) | Critical? | Serves | Acceptance check | Slice |
|----|---------|--------------|:--:|--------|------------------|:--:|
| F1 | Multi-language AST parse | embed tree-sitter + grammars; file → CST | ✅ | SC-1/3, Job | each fixture lang parses to a symbol tree; unknown lang → clear error | S1 |
| F2 | Symbol/definition extraction | tags query → funcs/methods/classes w/ FQ-name, file, span, signature, docstring | ✅ | SC-3, Job | every defined fn in fixture appears as a node w/ correct span+signature | S1 |
| F3 | Call-site + import extraction, alias-aware (universal edges) | `@reference.call` + import parse → name-based caller→callee edges; resolve import aliases `→ Design` | ✅ | SC-3, N1 | aliased call attributed to correct callee (no silent gap); name-collisions flagged `ambiguous` | S1 |
| F4 | Python exact resolver (first per-lang plug-in) | scope/import/type-aware resolution behind a resolver seam; clears universal-tier ambiguity, esp. method calls `x.foo()` `→ Design` | ✅ | SC-4, N2 | 2 Python fns named `foo` → callers bind to the correct one; ambiguous flags cleared | S2 |
| F5 | Persistent on-disk graph store | embedded store holds symbols + edges + file hashes `→ Design/Stack` | ✅ | SC-1, A1 | graph survives process exit; reload is a read, not a re-parse | S1 |
| F6 | Delta / incremental update | on call: stat/hash files → re-parse only changed → patch affected nodes/edges `→ Design` | ✅ | SC-1, A1 | touch 1 file in 1k-file repo → only it re-parsed; graph == from-scratch rebuild | S1 |
| F7 | Closure query (direct callers + callees) | depth-1 graph traversal from target `→ Design` (depth) | ✅ | SC-3, Job | returns exactly the hand-verified direct caller+callee set (or safe over-set + flags at universal tier) | S1 |
| F8 | Enumeration query (counts + locations) | traversal → counts + file/line list, no bodies | ✅ | SC-3b, Job | "foo has N callers across M files" matches fixture ground truth | S1 |
| F9 | Bundle assembly | target + selected scope → JSON: target body, callee signatures, caller call-site snippets (±N lines), omission report `→ Design` (schema, snippet radius) | ✅ | Job, SC-4 | bundle has target body + all requested caller snippets + explicit omissions; schema-valid JSON | S1 |
| F10 | Token count + size report + overflow signal | count bundle tokens (target-model tokenizer/approx); emit size; scope > budget → overflow signal, never silent trim `→ Design` (tokenizer) | ✅ | SC-2 | emitted `token_count` within tolerance; over-budget scope → overflow signal, no silent drop | S1 |
| F11 | Omission / ambiguity / unresolved reporting | bundle carries `omitted[]`, `ambiguous[]`, `unresolved[]` | ✅ | N1, N2, A2, SC-3 | dynamic/reflection call → `unresolved[]`; pre-resolver name collision → `ambiguous[]` | S1 |
| F12 | CLI (stdout JSON) | subcommands `index` / `bundle` / `enumerate` / `status`; JSON→stdout, errors→stderr, exit codes | ✅ | Vision, Job | each subcommand emits schema-valid JSON; non-zero exit on error | S1 |
| F13 | Symbol targeting / lookup | resolve `module.func` / `file:funcname` / `file:line` → symbol node | ✅ | Job | each target-spec form resolves to correct node; ambiguous/missing → clear error | S1 |
| L1 | Exact resolvers beyond Python (Java/Go/TS/C…) | more per-lang plug-ins behind the F4 seam | ⬜ | "all needed" (not load-bearing for v1 SCs) | — | Later |
| L2 | Lazy Tier-2 LLM summaries | on-pull, content-hash-cached prose gloss | ⬜ | richer bundles (K4 — earn it) | — | Later |
| L3 | `--watch` daemon | long-running incremental indexer | ⬜ | latency at scale (K1) | — | Later |
| L4 | MCP server wrapper | expose queries over MCP (native Hector/Bob transport) | ⬜ | integration ergonomics (CLI+JSON suffices v1) | — | Later |
| L5 | Transitive / depth-N closure | configurable traversal depth > 1 | ⬜ | deeper blast radius (v1 = depth-1) | — | Later |
| L6 | Relevance ranking on overflow | rank callers when a scope overflows | ⬜ | v1 defers to Hector splitting instead | — | Later |

## Scope
<!-- phase: features -->
- **S0 — Validation spike (DONE for rate; `spike/`):** measured `exact`/`ambiguous`/`external` on real code. **Finding on pg-raggraph (user's own, method-heavy):** direct function calls resolve well (77% exact in-repo) but method calls `x.foo()` are 86% ambiguous by name alone — and pg-raggraph is 53% method calls. **This un-gated F4** (see below). S0.2 (SC-4 lift through Bob) remains, user's env.
- **In (critical, v1 = S1 + S2):**
  - **S1 — universal tier:** F1–F3, F5–F13 — end-to-end for all tree-sitter languages *at the universal tier* (over-inclusion + honest flags) (parse → store → delta → closure/enumerate → bundle → CLI). Independently shippable; sufficient alone for *function-heavy* codebases. Traces to SC-1/2/3/3b + Job.
  - **S2 — Python exact resolver (F4):** promoted from gated to v1-critical by the S0 finding. Type-aware resolution that clears method-call ambiguity, behind the resolver seam. Required for method-heavy/OO Python (the user's target) to make bundles trustworthy for the parallel-refactor use case. Traces to SC-4/N2.
- **Later (could):** L1 more per-language resolvers (Java/Go/TS/C… — remain ASK-FIRST K3, one at a time), L2 lazy LLM summaries, L3 `--watch` daemon, L4 MCP wrapper, L5 depth-N closure, L6 overflow ranking. Each serves a real want; no v1 SC is load-bearing on it.
- **Won't:** embedding/semantic search (violates N2); whole-repo dump/pack (code2prompt/repomix's job); editing/verifying (Bob) or task partitioning (Hector) (N3); editor/IDE/LSP plugin; cross-repo graph.

**Traceability check (both directions):** SC-1→F1/F5/F6; SC-2→F10; SC-3→F2/F3/F7/F11; SC-3b→F8; SC-4→F9 + F4 (both v1); Job→F7/F9/F12/F13. No SC/Job is unimplemented; no v1-critical feature traces to nothing.

**Scope decisions locked:** (a) closure **depth-1** each direction in v1, `--depth` knob defaulting to 1; transitive = L5 Later. (b) v1 = **S1 universal + S2 Python exact resolver** — S2 promoted into v1 by the S0 spike (method calls 86% ambiguous by name; user's code 53% methods). Resolvers *beyond* Python stay ASK-FIRST (K3). The **resolver seam** is designed in v1 so adding further resolvers needs no core rebuild.

## Design — How It Works
<!-- phase: design -->
_Grounding + open questions: `evidence/design.md`. All items below are specy's proposals accepted under "take your recommendations" — flagged so they can be revisited._

### D0 — The completeness invariant (SC-3 / N1) — *the core correctness story*
**Mechanism:** resolution **never deletes a call-site; it only labels it.** Every `@reference.call`
capture becomes an edge tagged exactly one of: `exact` (single resolved callee), `ambiguous`
(candidate callee set — *all kept*), or `unresolved` (dynamic/reflection/target-not-in-repo). A real
caller can be lost *only* by (a) failing to parse a file, or (b) discarding a capture — both forbidden,
both directly testable (file-coverage assertion + delta-vs-rebuild equivalence test). This turns
"no silent gap" from a hope into a structural property. **Why:** it's the reason the tool is
trustworthy enough for parallel refactoring. **Alternative rejected:** rank-and-truncate (Aider's) —
fails N1 by design.

### D1 — Data model & store (F5)
**Mechanism:** embedded **SQLite** file (`.maple/graph.db`). Tables: `files(path, hash, lang,
mtime, indexed_rev)`, `symbols(id, file, fq_name, kind, start_line, end_line, signature, docstring,
body_hash)`, `edges(caller_symbol, callee_symbol|null, callee_name, kind ∈ {exact,ambiguous,unresolved},
call_site_line)`, `imports(file, local_name, source_name, source_module)`, plus a `name → symbol_id`
index. **Why:** zero-ops, transactional patching, survives restart, queryable, single-file, no server
(honors K2, "tiny"). **Alternatives:** Neo4j/FalkorDB (Blar/Potpie) — rejected, heavyweight; pure
in-memory rebuild — rejected, loses the delta win. **Open:** whether huge repos want per-file
row-groups; defer until measured.

### D2 — Freshness / delta path (F6, SC-1) — *the <1s mechanism*
**Mechanism, warm call:** (1) open SQLite (mmap); (2) **detect changes cheaply** — if target is a git
repo, `git diff --name-only <indexed_rev>` + porcelain working-tree status = **O(changes)**; else fall
back to an mtime/hash walk; (3) re-parse only changed files (tree-sitter, parallel); (4) **two-pass
patch:** pass A updates changed files' definitions + outgoing call-sites; pass B re-resolves any edge
whose `callee_name` intersects the set of added/removed/renamed definition names (via the name index)
— this catches edges *from unchanged files* into a renamed symbol, preserving correctness; (5) query +
assemble. **Why:** the git-aware step is what makes freshness O(changes) not O(files) on a 100k-file
repo — the difference between <1s and seconds. **Alternative:** always full mtime walk — rejected as
the SC-1 risk on huge repos (kept as the non-git fallback). **Open:** cold-index wall-clock target →
Stack.

### D3 — Universal-tier resolution + import aliases (F3) + resolver seam (F4)
**Mechanism:** for each `@reference.call` name at a call-site, resolve via: (a) local import map —
expand aliases (`import bar as baz; baz()` → `bar`) and qualified paths; (b) look up candidate
definitions by resolved name in the `name → symbol_id` index. 0 candidates → `unresolved`; 1 →
`exact`; >1 → `ambiguous` (keep all, flag). **Resolver seam:** a `Resolver` interface
`resolve(call_site, candidates, file_ctx) -> Resolution{exact|ambiguous|unresolved}`. The universal
resolver is the default; per-language exact resolvers (F4, now v1) plug in to *narrow* `ambiguous`→`exact`
— they may never *widen* or *drop* (preserves D0). **Why:** alias expansion is the one thing pure
name-matching needs to avoid under-inclusion (a silent gap); everything else over-includes safely.
**Open:** in-process trait vs. out-of-process subprocess protocol for heavy per-lang resolvers → Arch.

### D4 — Bundle schema & assembly (F9), snippet radius
**Mechanism:** `bundle` takes a target + a selected scope (all direct edges, or a Hector-chosen subset)
→ JSON:
```json
{ "target": {"fq_name","file","span","signature","docstring","body"},
  "callees": [{"fq_name","signature","file","span","resolution"}],
  "callers": [{"fq_name","call_site":{"file","line","snippet"},"resolution"}],
  "report":  {"token_count", "budget", "over_budget": false,
              "omitted": [], "ambiguous": [...], "unresolved": [...]},
  "meta": {"depth":1,"generated_from_rev","tokenizer"} }
```
Caller entries carry the **call-site snippet** (±3 lines, tunable) — the deterministic "how is it used"
that beats prose (Tier-1; Tier-2 LLM gloss stays K4-gated). Callees carry signatures, not full bodies,
unless depth requests otherwise. **Why:** the snippet is the highest-signal, lowest-token evidence.
**Open:** ±3 line radius is a guess — tune against SC-4.

**⚠ Fan-in bound (S0.2 finding).** Bundle size scales with **fan-in, not file size**. The S0.2 harness
showed god-file symbols shrink 96–98% (33K-tok file → ~1K bundle, doesn't-fit → fits), but a symbol with
39 callers produced a bundle *larger than its small file*. So including *all* callers by default is wrong.
**Default:** cap caller inclusion (e.g. first K by locality) and always report the *full* fan-in count via
enumeration (SC-3b) so Hector pages the rest into slices — callers are the blast-radius view you scope,
callees are the understand-the-target view you include. Never silently drop (N1): capped callers go in
`report.omitted` with the total count. Evidence: `spike/README.md`.

### D5 — Token budgeting (F10)
**Mechanism:** pluggable tokenizer; **default = fast conservative approximation** (byte/char-based BPE
estimate) with a safety margin, since the budget already has headroom (16K of 32K) and SC-2 needs a
*bound*, not exactness. Optionally load the target model's real tokenizer for precision. Assembly emits
`token_count`; if the *requested scope* exceeds budget it sets `over_budget:true` and returns the full
scope anyway (**never a silent trim** — Hector's cue to split). **Why:** exact per-model token counting
is a rabbit hole; a conservative over-estimate + margin satisfies SC-2 safely. `tiktoken` is
OpenAI-exact only, so it's just one optional plug-in, not the default. **Open:** margin size → tune.

### D6 — Closure depth (F7)
**Mechanism:** depth-1 each direction by default (`--depth`, default 1). Target + direct callees
(signatures) + direct callers (call-site snippets). Transitive = L5 Later; the agent/Hector re-queries
if a slice needs more. **Why:** depth-1 is the smallest thing that answers "what does it call / who
breaks"; deeper is speculative bloat against SC-2.

## Tech Stack
<!-- phase: stack -->
**Rust core + SQLite (`rusqlite`) store + tree-sitter (`tree-sitter` crate) + `rayon` for parallel re-parse. Out-of-process resolver seam** so future per-language exact resolvers can be written in any language (e.g. a Python resolver shelling to PyCG) without touching the core.

- **Why Rust:** ecosystem fit (Bob + Hector are 100% Rust — same toolchain, shareable crates); single static binary with no runtime/interpreter cold-start, which SC-1 (<1s, run before *every* dispatch) demands; mature tree-sitter bindings and `rusqlite`; clean data-parallelism via `rayon`.
- **Store:** SQLite — zero-ops, transactional patching, survives restart, single file, no server (honors K2 "tiny/daemonless").
- **Tokenizer:** pluggable; default fast approximation (D5), `tiktoken`-style exact counters are optional plug-ins only.

### Decision log
- **Language = Rust.** Alternatives: **Go** (single binary, simpler concurrency, but weaker tree-sitter story and *mismatched* with the Rust pipeline) — rejected; **Python** (fastest to prototype, could reuse PyCG) — rejected as the *core* (interpreter startup threatens SC-1, "tiny binary" packaging is painful, ecosystem mismatch), but *preserved* via the out-of-process resolver seam for per-language resolvers.
- **Store = SQLite.** Alternatives: **Neo4j/FalkorDB** (Blar/Potpie) — rejected, heavyweight, violates K2; **in-memory rebuild** — rejected, loses the delta/freshness win.
- **Resolver seam = out-of-process protocol** (vs. in-process trait). Chosen so heavy/foreign-language resolvers don't bloat or destabilize the core; the in-process trait remains an option for lightweight resolvers. (Confirmed open item from D3.)
- _Unverified:_ no published large-repo cold-index wall-clock baselines exist among competitors (SC-1 sets its own bar); cold-index target to be benchmarked, not matched.

## Architecture
<!-- phase: architecture -->
Single Rust binary, stateless process over a durable SQLite store. No daemon.

```
                              maple (CLI binary)
  ┌──────────────────────────────────────────────────────────────────┐
  │  cli        parse args → subcommand → JSON/stdout, err/stderr,exit │
  │   │                                                                │
  │   ▼                                                                │
  │  delta   ── change detect (git diff <rev>  |  mtime/hash fallback) │
  │   │            │                                                   │
  │   │            ▼                                                   │
  │   │        parser  (tree-sitter + tags.scm per lang)              │
  │   │            │  defs / call-sites / imports (syntactic)         │
  │   │            ▼                                                   │
  │   │        resolver  ── universal (name + alias) ──┐              │
  │   │            │        [seam] per-lang exact ─────┘ (out-of-proc)│
  │   │            ▼                                                   │
  │   └────────►  store (SQLite: files,symbols,edges,imports,name-idx)│
  │                    ▲            │                                  │
  │        graph ──────┘            │  closure(depth-1) / enumerate    │
  │          │                                                         │
  │          ▼                                                         │
  │        bundle  ── assemble target+callees+callers+snippets        │
  │          │        + budget (tokenizer) + omission/ambiguity report│
  │          ▼                                                         │
  │        JSON bundle → stdout                                        │
  └──────────────────────────────────────────────────────────────────┘
   State lives in ./.maple/graph.db  (durable; process is stateless)
```

**Components:** `cli` (interface), `delta` (freshness orchestration), `parser` (tree-sitter → syntactic facts), `resolver` (name-resolution + plug-in seam), `store` (SQLite persistence), `graph` (closure/enumeration traversal), `bundle` (assembly), `budget` (pluggable tokenizer).

**Data flow (warm `bundle` call):** `cli` → `delta.refresh()` [detect changes → `parser` re-parses only changed files → `resolver` re-labels affected edges → `store` two-pass patch] → `graph.closure(target, depth=1)` → `bundle.assemble(scope, budget)` → JSON on stdout. Cold `index` is the same pipeline over all files, once.

**Where state lives:** entirely in `./.maple/graph.db`. The process holds nothing between invocations; every call self-heals to current truth (A1). This is why no daemon is needed.

**Seams a coding agent builds against (the writing-plans contract):**
1. **CLI + JSON schemas** — `index [path]`, `bundle --symbol <spec> [--depth N] [--budget 16k] [--scope <edge-selector>]`, `enumerate --symbol <spec>`, `status`. Stable stdout schemas (bundle schema = D4; enumerate = counts+locations for Hector). This is the *external* contract for Hector/Bob.
2. **`Resolver` seam** — `resolve(call_site, candidates, file_ctx) -> Resolution{exact|ambiguous|unresolved}`; universal resolver default; per-language exact resolvers plug in and may only *narrow* (D0 guarantee), out-of-process.
3. **Language registration** — add a tree-sitter grammar + `tags.scm` to enable the universal tier for a new language.
4. **Store schema** (D1) — the internal data contract all components share.
5. **Tokenizer plug-in** (D5) — the budget seam.

**Target-spec grammar** (F13): `module.func` | `path/file.py::func` | `path/file:line`. Ambiguous/missing → non-zero exit + JSON error on stderr.

## Persona Feedback
<!-- phase: personas -->
_Full voicing: `evidence/persona-feedback.md`. Four personas, each with one real objection._

- **P1 — Pipeline Operator (primary user):** loves it, but "SC-4 target is TBD — I could build all of S1 and *then* find the small tier still can't use the bundles." → wants an **early validation spike**. *Verdict: In, conditional.*
- **P2 — Integrator (builds against the CLI):** "'No silent drop' is only as good as `@reference.call` coverage. If 40% of a legacy file's calls are `unresolved`, the bundle is *honest and useless*. **Honesty ≠ usefulness.**" → needs a **measured unresolved rate on real code**. *Verdict: Conditional.*
- **P3 — Competitor/Critic (AAT):** "This is Aider's repo map with a flag. You **deferred the moat** (exact resolution) out of v1. What does v1 do that `aider --map-tokens` doesn't?" *Verdict: Skeptical.*
- **P4 — External OSS Adopter (Java legacy):** "'All tree-sitter languages' **oversells v1** — exact resolution is Python-first/gated, so for Java I get over-inclusion I can't trust for a fan-out." *Verdict: Watch, not adopt.*

**Resolutions (carried into synthesis):**
1. **P3 (the load-bearing one):** v1's differentiation is *not* exact resolution — it's four things Aider structurally can't do: completeness **contract + omission reporting** (Aider truncates silently), an **enumeration API** for a planner, **always-fresh delta as a headless CLI** (Aider's map is welded to its chat), and a **composable budget bundle**. For *automated parallel* refactoring, "over-include + honest flags" beats "silent truncate." Different product, not a worse Aider.
2. **P1 + P2 converge → add an early validation spike** as the *first* work item: measure unresolved/ambiguity/over-inclusion rate + SC-4 lift on one real target repo *before* the full S1 build.
3. **P4 → soften "all languages"** everywhere to "all languages at the *universal tier* (over-inclusion + honest flags); *exact* resolution per-language, gated." No overclaim.

<!-- phase: synthesize — synthesis distills the sections above; no new section. -->
