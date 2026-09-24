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
