# WAVE L1 SPEC — Multi-language universal tier (Rust, C, C++, C#, Java, JS, TS, Go)

Repo `<repo>` = /Users/matt.yonkovit/yonk-tools/maple. `source ~/.cargo/env`. Baseline: all tests
green, clippy -D warnings clean. Constraints unchanged: D0 (every call-site = exactly one edge,
labels exactly `exact|ambiguous|unresolved`), delta==rebuild equivalence, never guess, clippy stays
clean, CI must still pass.

## Scope
UNIVERSAL TIER ONLY for the 8 new languages: defs, call-sites (func vs method kind), imports/aliases,
name-based resolution. NO per-language exact resolvers (Python keeps its S2 resolver; receiver-class
hints for new langs ONLY where syntactically free — e.g. Go method receivers, Rust `impl` blocks —
never inferred). Python behavior must be byte-identical (pg-raggraph gate below).

## L1.1 — Language registry
Refactor `parser.rs`: a registry mapping extension → (tree-sitter Language, walk fn) producing the
existing `ParsedFile` shape. Registered: `.py` (existing), `.rs`, `.c`/`.h`, `.cpp`/`.cc`/`.hpp`/`.hh`,
`.cs`, `.java`, `.js`/`.jsx`/`.mjs`/`.cjs`, `.ts`/`.tsx` (tree-sitter-typescript exposes two grammars —
use TYPESCRIPT for .ts, TSX for .tsx), `.go`. Generalize the file walkers in `store.rs`
(`python_files`, refresh disk walk, suspect logic) to all registered extensions. `maple parse` error
message updated ("unsupported language ... supported: ...").
Grammar crates: tree-sitter-{rust,c,cpp,c-sharp,java,javascript,typescript,go} — pick versions
ABI-compatible with the workspace `tree-sitter` (currently 0.25; bump tree-sitter itself if the
grammar set needs it, keeping tree-sitter-python compatible). Getting this version matrix to compile
is part of the job; document final pins.

## L1.2 — Language-scoped resolution (CORRECTNESS — the one schema change)
`symbols` gains a `lang` column (copy from the file's language at insert). `resolve_call` (and the
pass-B relabel, and F3 import filtering, and T4 base-class hop, and the receiver-class validation)
filter candidates to the CALLER's language. A `.rs` call must never match a `.java` def. Schema bump
via the existing `SCHEMA_VERSION` mechanism (auto-reset handles old dbs). Cross-language calls (FFI,
subprocess) are honest `unresolved`.

## L1.3 — Per-language walks (keep each shallow, honest, ~150–250 lines)
For each language extract:
- **defs**: functions/methods (+ classes/structs/impl-types as `class`-kind containers where the
  concept exists). `parent_class` = enclosing class / interface / impl-type / Go receiver type name.
  Signature = first line; docstring where cheap (Rust `///`, Java/C#/JS/TS leading `/** */` first
  line) else None.
- **calls**: plain calls → kind `func`; member/selector/method calls → kind `method` with the member
  name. Receiver-class hints ONLY where free: Go method receiver in same type's methods (`s.foo()`
  where s is the receiver ident → receiver type), Rust `self.foo()` inside an `impl T` → T. Others: None.
- **imports/aliases**: Rust `use a::b as c`; Go import aliases; Java imports (last segment); C# using;
  JS/TS `import {a as b} from`, default imports; C/C++ `#include` recorded as raw imports only (no
  alias semantics). Alias expansion feeds the existing resolution path unchanged.
Language notes: C has no methods (all `func`). C++: methods via class bodies + qualified
`X::y` definitions where cheap; stay shallow — C++ is the one allowed to over-report `unresolved`
rather than grow clever. JS/TS: function_declaration, class method_definition, arrow fns bound via
`const x = () =>` (that one is cheap and very common); TSX/JSX same walk.

## L1.4 — Tests + fixtures (per language, tempdir style like existing)
Each language: one fixture pair (two files) proving — a cross-file `func` call resolves exact; a
method call lands kind `method`; an import-alias call binds (where the language has aliases); defs
carry parent container. Plus: one polyglot fixture proving language scoping (same fn name in .rs and
.py → each caller resolves ONLY to its own language's def, no cross-lang ambiguity). Extend the
delta==rebuild equivalence test to a mixed-language repo. N2 label-domain test unchanged.

## Gates
- `cargo test` fully green; `cargo clippy --all-targets -- -D warnings` clean; CI workflow untouched.
- **Python regression gate:** delete pg-raggraph `.maple`, reindex → exactly
  14484 edges / 4441 exact / 2029 ambiguous / 8014 unresolved.
- **Rust dogfood measurement:** index /Users/matt.yonkovit/yonk-tools/bob and .../maple themselves;
  report files/symbols/edges + per-kind resolution split for each. Spot-check one real closure
  (e.g. `maple closure . --symbol resolve_call` in the maple repo) and confirm callers look sane.
- `maple parse` on a supported non-Python file returns symbols; on `.txt` errors clearly.
- README: update the language section (v1.1: 9 languages universal tier; Python only has the exact
  resolver; table of what "universal tier" means per language).

## Report back
Final dependency pins · per-language walk line counts · all gate outputs (Python gate EXACT, Rust
dogfood numbers) · per-language fixture test list · judgment calls/deviations · anything deferred.
