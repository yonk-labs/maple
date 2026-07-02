# Alignment Check — maple (code-symbol-graph)

**UPDATE 2026-07-01 (post-build):** v1 is code-complete; this doc now records *implementation*
evidence. SC-1 ✅ (p95 13ms measured) · SC-2 ✅ (budget report + over_budget signal + fan-in cap,
tested) · SC-3 ✅ (D0: edges==call-sites on real code; delta==rebuild equivalence tests) · SC-3b ✅
(enumerate verified vs ground truth) · **SC-4 ⏳ open** (needs Bob pipeline — the one remaining item).
S2 Python resolver shipped: method ambiguity 86%→65% on pg-raggraph, narrowing-only. N2 has a
positive test (`n2_label_domain_is_closed`). 9/9 tests green. See PLAN.md "Definition of done".

---

Original pre-build gate below (spec-internal alignment).
Run: SC → feature → plan task → test, plus principle→guard, plus contradiction scan.

## SC traceability (every SC has a feature, a plan task, and a test)
| SC | Feature(s) | Plan task | Test / evidence | Status |
|----|-----------|-----------|-----------------|:--:|
| SC-1 speed <1s warm | F1, F5, F6 | S1.1/1.2/1.6 | S1.7 latency benchmark (p95<1s) | ✅ traced |
| SC-2 ≤16K, self-report, no silent trim | F10 | S1.5 | token-count assertion harness | ✅ traced |
| SC-3 completeness, no silent gap | F2, F3, F7, F11 | S1.1/1.3/1.4/1.5 | god-function fixture + file-coverage assert + delta-vs-rebuild equivalence | ✅ traced |
| SC-3b enumerability | F8 | S1.4 | enumeration ground-truth test | ✅ traced |
| SC-4 sufficiency (true-north) | F9 + F4 (both v1) | S0.2, S1.5, S1.7, S2.1 | end-to-end through Bob | ⚠ target TBD until S0.2 (by design); F4 un-gated into v1 by S0.1 finding |
| Job (bundle completes task) | F7, F9, F12, F13 | S1.4/1.5 | e2e via Bob | ✅ traced |

## Principle → guard (every NEVER/ALWAYS has something enforcing it)
| Principle | Guard | Status |
|-----------|-------|:--:|
| N1 never silent drop | D0 invariant + S1.3 file-coverage assert + S1.6 equivalence test | ✅ tested |
| N2 never fuzzy/guess | edges only ever exact/ambiguous/unresolved; S1.5 asserts no similarity/confidence score on any edge | ✅ tested |
| N3 never orchestrate/build | scope excludes; no plan task builds it (DC-1) | ✅ by scope |
| A1 fresh at query | D2 delta + S1.6 equivalence test | ✅ tested |
| A2 report ambiguity/unresolved | F11 + dynamic-dispatch fixture | ✅ tested |
| A3 universal tier all langs | F1 + multi-lang fixtures | ✅ tested |
| A4 emit token size | F10 + token-count assertion | ✅ tested |

## Contradiction scan
- ✅ Fixed in synthesis: Differentiation's old "v1 = one language" now reconciled with "universal tier all langs + gated resolvers."
- ✅ SC-2 (size) vs SC-3 (completeness) — resolved architecturally (Hector partitions; completeness in graph, size per-slice). Consistent throughout.
- ✅ No remaining template placeholders.

## Deliberate open items (acknowledged, not defects)
1. **SC-4 target = TBD** — S0 sets the baseline+target. This is the point of the S0 kill-gate.
2. ~~**N2 has no positive test**~~ — ✅ RESOLVED: added to S1.5 acceptance (assert every edge carries
   an exact|ambiguous|unresolved label, never a similarity score). All NEVER/ALWAYS now guarded.
3. **Cold-index wall-clock target unset** — record in S1.7, set target then (Stack notes it unverified).
4. **SC-1 <1s has no external baseline** — self-set bar (research confirmed no competitor publishes one).
5. **SC-3 completeness = depth-1** by definition; transitive completeness is explicitly L5 (out of v1).

## Verdict
Spec is internally aligned: no orphan SC, no orphan v1-critical feature, no contradictions.
One recommended pre-build tweak (open item #2). Re-run the *code-based* `/verify-alignment`
after S0/S1 land — that's when SC evidence exists to check against.
