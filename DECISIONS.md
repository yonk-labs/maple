# Decisions

Locked decisions, newest last. Check here before changing a behavior that looks like one of these.

## 2026-09-23 — Which files are indexed: git's view of the repo
Inside a git repo the file set is `git ls-files -co --exclude-standard` (honors .gitignore, skips
nested repos/worktrees), minus SKIP_DIRS, minus files .gitattributes marks `linguist-generated` /
`linguist-vendored`. Outside git, or when git lists no source files, the directory walk runs (the
latter with a stderr notice). Full index and refresh apply the same rule; `meta.file_scope` stamps
the rule version so an older store gets one cleanup walk after an upgrade.
**Why:** real repos carried 30k+ junk files (`.claude/worktrees`, venvs, Unity `Library/`, vendored
tree-sitter tables): 2 GB / 191 s and 27 GB / 105 s indexes, and duplicate defs turning exact edges
ambiguous. Measured on 18 repos in ~/yonk-apps and ~/yonk-tools: every dropped file was junk.

## 2026-09-23 — A guessed exact is worse than an honest ambiguous
A method call with no trusted receiver hint used to resolve exact whenever the repo had exactly one
def of that name, so `s.clone()` bound to the repo's only `fn clone` (26% of pg-retest's exact
edges were this). Now a lone candidate on a per-language std/runtime method name (`clone`, `len`,
`join`, `execute`, `TryGetValue`, `$`, …) is `ambiguous`; project-specific names stay exact, and a
trusted hint (`self.len()` in `impl T`) still binds. Python bare builtins (`print`, `len`) not bound
by an in-repo import or a module-level def in the same file resolve to the builtin → `unresolved`
(LEGB scoping, not a heuristic).
**Chosen over:** an import/evidence gate on the candidate's owner (catches unlisted collisions but
demotes correct indirect calls; the upgrade path), and "never exact without a hint" (guts TS/JS).
**Precedent:** codegraph-external and codebase-memory-mcp ship per-language builtin tables for the
same problem. Exact counts drop on purpose; D0 (every call-site kept, exactly one label) is intact.

## 2026-09-23 — Rust receiver hints
`T::y()` / `m::T::y()` / `Self::y()` carry T as the hint (the type is in the call: syntactically
free). A hint naming a trait never narrows (`Trait::m(&x)` dispatches to the implementor), detected
fail-safe: only a stored first line that shows struct/enum/union narrows. A hint naming a std
concrete type (`Vec`, `String`, `Arc`, …) with no in-repo reach is `unresolved`, never traits
(`Default::default()` can dispatch into repo impls).

## 2026-09-23 — More syntactically free receivers (Python modules, `this`, JS globals)
- Python `m.f()` where `m` is import-bound in the file (`import m`, `import a.b as m`, `from p import
  m`, incl. `a.b.f()` chains): the module's own file -> its module-level def; `from pkg import Cls`
  -> Cls's methods; no prefix of the module path in the repo -> external -> unresolved; an in-repo
  package that doesn't define it (re-export) -> the old universal answer, never a guess.
- `this.m()` / `this->m()` in TS/JS/Java/C#/C++: the enclosing class is the hint, as Rust's `self`.
  JS `function(){}` rebinds `this` (no hint); arrows keep it. The class's first base is recorded, so
  an inherited method resolves one hop up (T4). A Java/C#/C++ constructor named like its class
  doesn't make the hint ambiguous (it blocked 1,306 of godot's 1,335 C++ hints).
- JS/TS built-in globals (`Promise.all`, `Math.max`, `console.log`) -> unresolved, unless the file
  declares that name anywhere (scope-blind on purpose: conservative).
**Measured on 18 repos vs 0.3.10:** exact +984, ambiguous -6,725 (e.g. Excalibur TS exact +495,
bento Python ambiguous -1,954); every changed edge sits at a call site carrying one of these hints,
and no call-site was lost.

## 2026-09-24 — JS/TS imports, scope-aware Python shadowing
- JS/TS bare `foo()` and `ns.foo()` bound by a relative import resolve in the imported file (dir ->
  `index.*`, TS-ESM `./x.js` -> `x.ts`). A package import is declared external ONLY for `node:*`,
  Node core modules and a short well-known list (vitest, jest, react, ...) with no same-named repo
  dir. Any other bare specifier can be a tsconfig alias or the repo importing itself (Excalibur's
  tests import `'@excalibur'`: a "no such dir -> external" rule turned 1,542 correct exacts into
  unresolved) -> the universal answer stands. `import {a as b}` gets no import narrowing (the
  resolver sees the post-alias name).
- Python module-hint shadowing is scope-aware: a rebinding hides the module only in its own
  function and functions nested in it (closures), a class body only for calls directly in it, and
  module-level / `global` rebinding everywhere. Recovers most hints the file-wide rule dropped
  (headroom 9,962 -> 10,007 of a 10,033 no-check ceiling); the gate's shadowing cases stay covered.
- DEFERRED (pending a call): walking inheritance beyond one hop; T4 stays one hop. Measured on 18
  repos it would resolve ~20 more edges (Excalibur 17, godot 2, Python 0): godot's many unresolved
  `this->` calls are a vendored header's macro-suffixed methods the C++ extractor can't see
  (`reflect() const VULKAN_HPP_NOEXCEPT`), not deep inheritance.
- Local receiver types (Rust, TS/JS, Java, C#, C++), Python T1's rule for the other languages:
  `x.m()` narrows to T when every binding of `x` in the enclosing function names the same T.
  Bindings: explicit declared types / annotations / typed params (`T`, `&T`, `T*`, `const T&`,
  `a::T`), `new T(..)` (incl. `var` / `auto`), Rust `T { .. }` and constructor-named associated
  calls only (`T::new*`, `with_*`, `from*`, `default`, through `?` / `.unwrap()` / `.expect()`).
  Generic containers and other associated fns (`T::open()` may return `Result<T>`) never bind.
  JS/TS reassignment `x = new U()` with U != T voids the hint (runtime dispatch). Any other
  reassignment voids it in plain JS (no static type) but not in TS, whose checker keeps the
  declared/inferred type (voiding there cost Excalibur 540 correct exacts on `engine = ...` setup).
  Java/C#/C++ locals resolve against their declared static type, the `this`/`self` convention.
  This batch (JS imports + scoped shadowing + local types) vs 0.3.11 on 18 repos: exact +12,019,
  ambiguous -12,585 (godot +7,372, Excalibur +3,112, the-horde +505, chunkshop-rust-media +251);
  every changed edge sits at a site one of these rules touches, no exact edge lost, edge totals
  unchanged; godot indexing CPU time unchanged (51-53 s both).
