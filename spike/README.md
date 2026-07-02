# S0 spike — resolution-rate measurement (THROWAWAY)

Answers `spec/code-symbol-graph/PLAN.md` **S0.1**: before building the Rust core, measure how well
name-based (universal-tier) resolution actually does on real code. This is the kill-gate.

## Run
```bash
python3 -m venv .venv && . .venv/bin/activate
pip install "tree-sitter>=0.23" tree-sitter-python
python3 s0_resolution_rates.py /path/to/your/real/legacy/repo --top 25
# logic self-check (no deps): python3 s0_resolution_rates.py --selfcheck
```
Add languages by adding a loader to `LANG_LOADERS` (same shape as `_load_python`).

## Buckets
- **exact** — 1 in-repo def with that name → clean single bundle edge.
- **ambiguous** — >1 in-repo defs → universal tier over-includes (safe, flagged). What the exact resolver (F4) removes.
- **external** — 0 in-repo defs → stdlib / third-party / aliased import / dynamic. Mostly NOT a gap.

## Data point 1 — CPython 3.14 stdlib (pre-sharpening; funcs+methods lumped, no class index)
exact 22.6% / ambiguous 55.6% / external 21.8%. In-repo: 71% ambiguous. Near worst-case (huge method-name
collision). Motivated sharpening the spike (index classes; split func vs method calls).

## Data point 2 — pg-raggraph (309 files, 14,484 call-sites) — SHARPENED
| call kind | share | in-repo resolution |
|-----------|------:|--------------------|
| **function `foo()`** (tractable) | 47% | **77% exact / 23% ambiguous** ✅ |
| **method `x.foo()`** (needs receiver type) | 53% | **14% exact / 86% ambiguous** ❌ |
| combined | — | 44% exact / 56% ambiguous |

External (54%) is now clean after indexing classes: `len`, `print`, `append`, `add_argument`,
`echo`(click), `raises`(pytest) — builtins/stdlib/third-party, i.e. legitimate non-gaps.
`GraphRAG`/`PGRGConfig` correctly dropped out of "external" once class defs were indexed.

**Findings:**
1. **Function-call resolution is strong** — 77% exact in-repo. The universal (name-based) tier handles
   direct calls well; SC-3 holds and noise is low.
2. **Method-call resolution is weak** — 86% ambiguous in-repo. Name alone can't pick which class's
   method. This is precisely where the type-aware exact resolver (F4) earns its keep.
3. **pg-raggraph is method-heavy (53% of calls).** So for THIS codebase, most calls land in the weak
   bucket → name-based bundles would be noisy, and for a method-rename fan-out (P3's use case) the 86%
   ambiguity is actively unsafe without disambiguation.

**Implication for the spec:** F4 (Python exact resolver) is **load-bearing for method-heavy code, not a
gated afterthought.** Recommend elevating it from "gated fast-follow" to v1-second-slice. Function-heavy
codebases could ship on the universal tier alone; OO/method-heavy ones need F4.

## S0.2 harness — `s0_2_bundle.py` (depth-1 bundle prototype)
Assembles a depth-1 bundle (target body + callees + caller snippets + report + approx token count) for a
named symbol, as JSON. Prints a bundle-vs-whole-file token A/B to stderr. Reuses the rate-spike's
tree-sitter shims. Run: `python3 s0_2_bundle.py <repo> --symbol <name>` (`--selfcheck` for the logic test).

### Bundle-size A/B on pg-raggraph (token approx = chars/4)
| target | where | callers | bundle | whole file | result |
|--------|-------|--------:|-------:|-----------:|--------|
| `_as_aware_utc` | `__init__.py` (2928 L god-file) | 3 | ~683 | ~33,250 | **98% smaller** ✅ |
| `_living_bucket` | `__init__.py` god-file | 5 | ~1,226 | ~33,250 | **96% smaller** ✅ |
| `_validate_namespace` | `__init__.py` god-file | 13 | ~1,401 | ~33,250 | **96% smaller** ✅ |
| `embed` | `embedding.py` (129 L, small) | 39 | ~4,220 | ~1,555 | **2.7× BIGGER** ⚠ |

**Findings (both matter):**
1. **Premise validated for the real use case.** The god-file `__init__.py` is ~33K tokens — it *alone
   overflows a 32K window* (you literally cannot feed it). Bundles for its symbols are 683–1,401 tokens:
   doesn't-fit → fits-with-room. This is exactly what maple is for, now with numbers.
2. **Bundle size scales with FAN-IN, not file size.** `embed` (39 callers, tiny file) produced a bundle
   bigger than its own file. → including *all* callers by default is wrong for high-fan-in symbols. The
   caller set must be **scoped/paginated by the planner** (confirms why SC-3b enumeration + SC-2 per-slice
   bundles exist). Fed back into SPEC D4: default caller cap + enumeration of the full fan-in.

### S0.2 remaining (user's environment)
Feed these bundles through Bob to the ≤32K tier; measure VERIFY pass rate + "need more context" vs
whole-file. The rates say the resolver *matters* and bundles *shrink god-files*; this says *how much it
moves SC-4*. Needs the Bob pipeline.

## Known ceilings (ponytail)
- No import-alias expansion → some `external` are really in-repo aliased calls (inflates `external`).
  Upgrade path: add alias expansion, watch `external` drop — quantifies D3's value.
- `foo()` and `x.foo()` counted together → next refinement is splitting them to separate the tractable
  (function) from the hard (method) case.

## SC-4 run log (2026-07-01, autonomous session)
**Stack assembled on this machine:** bob 0.2.11 (built from source) · goose 1.39.0 · ollama @ 127.0.0.1
(gemma4-32k = gemma4:latest + num_ctx 32768; qwen3.6:35b-mlx) · VERIFY = pg-raggraph's own
`test_living_knowledge.py` on a scratch copy with an injected bug (week bucket anchored to Tuesday).
maple bundle for the buggy `_living_bucket`: **1,283 tok**; whole god-file: **33,283 tok (> 32K window)**.

**Bob e2e (plumbing): WORKS.** `bob build` with the maple bundle embedded in the task ran the full
loop — tier routing → goose → worktree → 2 iterations → honest `EmptyDiffAfterCritique` verdict,
`applied=false`. Nothing applied to the tree without verification. Scope caps + frozen-test contract
enforced in the builder prompt.

**Builder finding (environment, not maple):** both local models are *thinking* models; via goose's
OpenAI-compat path nobody sends `think:false`, so the visible response is empty → goose makes zero
tool calls → empty diff. Proven on the native API: same prompt returns response_len=0 with 600
thinking tokens, and a correct fenced fix with `think:false`. **Fix for the real pipeline:** use a
non-thinking coder model as the goose builder (e.g. bob's example `Qwen3-Coder`/qwen2.5-coder), or a
goose provider config that disables thinking. Tool-calling competence, not code quality, is the
gating skill for the builder path.

**SC-4 A/B result (direct-to-model, VERIFY = real pytest, gemma4-32k local):**
| arm | context | prompt tok | pass |
|-----|---------|-----------:|:----:|
| A — maple bundle | target + callees + caller snippets | 1,311 | **2/3** |
| B — whole god-file | truncated at 32K window | 32,094 | **0/2** |

Bundle-fed local model fixed the real bug and passed the repo's own tests in 5–8s/gen; whole-file
failed every trial. Small n — baseline, not final target. Harness: `sc4_ab.py` pattern (think:false,
PYTHONPATH=src verify, restore-after-trial).

**Getting to 3/3 (follow-up runs):**
- gemma4-32k @ temp 0: **0/3** — greedy completion is deterministically the WRONG fix; sampling diversity
  is what found the right one. Temp-0 is the wrong knob for this model.
- **qwen2.5-coder:14b (32K): bundle 3/3 PASS** (7–15s/gen). Whole-file also 2/2 — under qwen's tokenizer
  the file = 27K tok and *fit*; the bundle's win there is economics (25× fewer prompt tokens, 11× faster
  first-gen), and impossibility returns whenever a file exceeds the window in the model's own tokenizer.
- Bob e2e still empty-diff with qwen-coder → traced below goose: **this machine's ollama returns
  `tool_calls: null`** even for base qwen2.5-coder with a proper tools array (model emits prose-JSON
  instead; both goose providers affected). Environment issue (Ollama.app install), not maple/bob/model
  choice. Fix on the serving side: newer ollama or vLLM with a tool-call parser on the DGX tier.
