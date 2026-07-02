# Competitive Landscape & Feasibility — code-symbol-graph (phase 5 evidence)

Source: background research agent (7 streams, ~14 web sources). Full findings below.

## Comparable table (condensed)

| Name | What | Freshness | Budget-fit bundle? | Exact closure? | CLI? |
|------|------|-----------|:--:|:--:|:--:|
| **Aider repo map** | tree-sitter tags → PageRank-ranked repo map | incremental (mtime diskcache) | Y (`--map-tokens`) | **N** — truncates by rank, drops symbols | partial |
| **Sourcegraph SCIP/LSIF** | compiler-accurate nav index | full per-commit | N | Y | Y |
| **stack-graphs** | incremental name-resolution scope graphs (GitHub nav) | incremental | N | **partial** — def↔ref, not call-edges; **archived Sept 2025**; Py/JS/TS only | Y |
| **tree-sitter tags** | per-file syntactic tags | per-file re-query | N | **N** — call *sites*, not resolved callee | partial |
| **universal-ctags** | definition index (~150 langs) | full | N | N (defs only) | Y |
| **LSP call hierarchy** | `incoming/outgoingCalls` (@3.16) | editor-time/live | N | **Y** exact | N (protocol) |
| headless LSP (multilspy/lsp-cli) | drive LSP w/o editor | live server | N | callers via `references` | Y/lib |
| **Glean (Meta)** | facts-DB, Angle queries | full+incremental | N | Y | Y |
| **Kythe (Google)** | build-integrated xref graph | build-integrated (heavy) | N | Y | Y |
| **Potpie** | AST→Neo4j + CrewAI agents | on-demand ingest | partial | partial→Y | Y |
| **Blar/blarify** | LSP/SCIP→Neo4j code graph for agents | incremental = *planned* | N | **Y** | Y |
| **Cursor indexing** | chunk→embeddings→vec DB | ~5-min sync | n/a | **N** (docs: no caller/callee) | N |
| **Continue `@codebase`** | embeddings+keyword+LLM rerank | incremental local | N | N | N |
| **chunkshop** *(user's own)* | ingest→chunk→embed→vec DB | incremental sync | partial | **N** (RAG similarity) | Y |
| **pg-raggraph** *(user's own)* | Postgres GraphRAG; AST code graph | ingest fast, **graph lags (out-of-band)** | N | **Y** (`code-impact` CTE) but stale | Y |
| code2prompt / repomix / gitingest | repo→one prompt/digest + token count | re-run | N (count only) | N | Y |

## 5 closest (ranked)
1. **Aider repo map** — same primitives, *inverse design* (rank+truncate, no closure). This tool = the closure Aider refuses to guarantee.
2. **Blar/blarify** — exact graph for agents, OSS, CLI — but Neo4j-heavy; incremental + budgeting are future work.
3. **pg-raggraph (yours)** — has exact closure already; its freshness lag is this tool's entire reason to exist.
4. **Headless LSP call hierarchy** — semantically exact & free, but per-language-server spin-up, no budgeting, no assembly.
5. **Potpie** — graph+agent packaged, but RAG-flavored, Neo4j-heavy, budget/depth undocumented.

## Differentiation (survived the weekend-clone counter-argument)
The empty quadrant no tool occupies = **all four at once**:
- (a) exact caller/callee closure for a *targeted symbol* + **explicit no-silent-drop / omission reporting** (SC-3)
- (b) **enumerability** — "X has N callers across M files" for a planner (SC-3b)
- (c) **select-to-fit** token budget, not dump-and-count (SC-2)
- (d) **guaranteed-fresh via cheap delta, no daemon** (SC-1)

Market splits and leaves it empty: exact-closure tools are heavy/lagging/human-nav (no bundle); bundle tools are approximate/whole-repo (no closure); RAG tools are similarity (closure isn't a goal, misses are silent).

**Moat = integration + guarantee + form-factor bet, NOT IP.** No secret algorithm. Defensibility is: (1) exact per-language closure, (2) proven completeness or named omissions, (3) fast & small enough to run before *every* dispatch, daemonless. Blar could add budgeting; LSP-drivers could add bundling — the bar is speed-of-execution + a verifiable correctness contract.

**Weekend clone?** Demo yes, real thing no. Every primitive is off-the-shelf (tree-sitter, tags, tiktoken, mtime cache — Aider proves it). But **no off-the-shelf lib gives incremental, cross-file, EXACT caller→callee resolution across multiple languages through one interface.** That integration layer is the multi-week build nobody ships.

## Key tensions / risks for later phases
- **⚠ SC-3 (exactness) vs SC-1 (fast/daemonless) are in real tension.** Exactness usually costs a heavy semantic pass (Kythe/SCIP/LSP) OR a lagging out-of-band build (pg-raggraph). Landing both at once *is* the hard, defensible engineering. → **Design (phase 8) + Stack (phase 9).**
- **⚠ Exact cross-file call resolution does NOT come as a multi-language library.** Options: build on `stack-graphs` (archived, 3 langs, def↔ref only — you derive call edges) OR normalize N per-language tools (PyCG/go-callgraph/java-callgraph/Jelly). → **This forces a v1 LANGUAGE-SCOPE decision (phase 7 Scope).** Almost certainly v1 = ONE language, done exactly.
- **⚠ tiktoken is OpenAI-exact only** — approximate for Claude/Llama/local models. SC-2 needs the *target model's* tokenizer or an accepted approximation margin. → Design/Stack.
- **No published competitor baseline for SC-1's <1s** — you set the bar, not match it.

## Prior art — build ON, don't reinvent
- **Reuse:** tree-sitter (parse, incremental, multi-lang), tags.scm/ctags (symbols — syntactic only), tiktoken/target tokenizer (budget), tree-sitter-graph (graph DSL), mtime/hash + SQLite (delta — Aider's proven pattern).
- **The hard layer (build/bolt-together):** exact cross-file caller→callee resolution + completeness/omission contract + budget-fit assembly. Per-language exact call graphs exist only single-language & heavy (PyCG, go/callgraph, java-callgraph, Jelly); no unified interface. **This is the product surface.**
