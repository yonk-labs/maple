# Persona feedback — code-symbol-graph (phase 11 evidence)

Four personas voiced against the full spec. ≥1 real objection each.

## P1 — The Pipeline Operator (primary user; the spec's author-persona)
**Wins:** kills frontier escalation; tiny/daemonless; always-fresh; composes into Bob/Hector.
**Objection (real):** "SC-4's target is *baseline TBD*. I could build the whole Rust S1 slice and
*then* discover the small tier still can't use the bundles. I need proof it beats 'just feed the file'
*before* I invest in the full build." → **Wants an early validation spike.**
**Verdict:** In — conditional on an early proof-of-value milestone.

## P2 — The Skeptical Staff Engineer / Integrator (builds against the CLI)
**Objection (real):** "Your 'no silent drop' guarantee is only as strong as tree-sitter
`@reference.call` coverage. Method calls via variables, decorators, dynamic dispatch, macros, codegen —
the universal tier honestly marks those `unresolved`. Fine. But if 40% of a real legacy file's calls
are `unresolved`, the bundle is *honest and useless*. **Honesty ≠ usefulness.** What's the unresolved
rate on real code, and where does 'honest' stop being 'helpful' (i.e., silently sink SC-4)?"
**Verdict:** Conditional — needs a measured unresolved/ambiguity rate on real code.

## P3 — The Competitor / Critic (AAT lens)
**Objection (brutal, fair):** "This is Aider's repo map with a completeness flag. The hard part —
exact cross-language resolution — you *deferred* (Python resolver gated, all others Later). So v1 ships
the *easy* half you admit over-includes, and files the hard half under 'fast-follow.' **You speced the
moat out of v1.** What does v1 do that `aider --map-tokens` + a grep doesn't?"
**Verdict:** Skeptical — demands v1's differentiation be real *without* the exact resolvers.

## P4 — The External OSS Adopter (legacy Java monolith; user #2)
**Objection (real):** "I came for exact caller/callee on Java. You say 'all tree-sitter languages,'
but exact resolution is Python-first and gated — so for Java I get over-inclusion + ambiguity flags,
which for a 200-caller fan-out means I *still* can't trust the split. **'All languages' oversells v1.**"
**Verdict:** Watch, not adopt — until a resolver for their language lands.

## Cross-cutting resolution (feeds synthesis)
- **Answer to P3 (the important one):** v1's differentiation is NOT exact resolution. It's four things
  Aider structurally cannot do: (1) completeness **contract + omission reporting** (Aider *truncates
  and never tells you*); (2) **enumeration API** for a planner (Aider has none); (3) **always-fresh
  delta as a headless CLI** (Aider's map is welded to its chat loop); (4) budget bundle as a
  **composable primitive**. For *automated parallel* refactoring, "over-include + honest flags" is
  categorically safer than "silently truncate." That's a different product, not a worse Aider.
- **Answer to P1 + P2 (they converge):** make the **first work item an early validation spike** —
  measure unresolved/ambiguity/over-inclusion rate + SC-4 lift on ONE real target repo *before* the
  full S1 build. If the unresolved rate is fatal, we learn it in days, not after the Rust core.
- **Answer to P4:** **soften the "all languages" claim** everywhere to "all languages at the universal
  tier (over-inclusion + honest flags); *exact* resolution is per-language and gated." No overclaim.
