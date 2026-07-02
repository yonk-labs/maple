# LAUNCH-FIX SPEC — pre-public-push batch (from user-test + prod-ready audits, 2026-07-02)

Repo `<repo>`. `source ~/.cargo/env`. Baseline: 48/48 tests green.
Constraints unchanged: D0, three labels, delta==rebuild equivalence, no new runtime deps.

## F1 — CRITICAL: atomic cold index (fixes concurrent-index duplication + SIGINT wipe)
`store.rs index_repo`: `self.clear()` currently runs un-transacted BEFORE the parse phase; the write
tx starts later. Fix: parse first (no db writes), then ONE transaction = clear + all inserts + meta.
Result: concurrent `index` runs serialize at the db (second tx sees committed first; final state =
one correct graph — write a test with two threads/processes if feasible, else two sequential
interleaved stores asserting no duplication is acceptable with a code comment explaining the tx
guarantee); SIGINT before commit rolls back to the previous graph instead of leaving it empty.
Also set `PRAGMA busy_timeout=10000` on open so concurrent writers wait instead of erroring.

## F2 — repo-path validation (silent-success footgun)
`Store::open`: error clearly if `repo_root` doesn't exist or isn't a directory
("repo path does not exist: <path>") BEFORE create_dir_all. Also map the `.maple`-is-a-file case to a
clear message (".maple exists but is not a directory"). Tests for both.

## F3 — `enumerate` honors the full target-spec grammar
Route `enumerate` through `lookup_targets` (like closure/bundle): resolve spec → bare name + def
filter; `defs` in the output = the resolved defs; caller counts keyed on the bare name (unchanged
semantics). `exists`/`surface` stay bare-string (document in README). Test: `enumerate --symbol
"Class.method"` and `"file.py::name"` return the right defs (update/add tests).

## F4 — method `fq_name` includes the class
`pkg.mod.Class.method` (insert parent_class segment when present) everywhere fq_name is computed.
Update affected tests.

## F5 — user-facing CLI copy
Remove ALL internal ticket codes (S1.x/T*/W2.x/D4/N2) from clap about/doc strings in main.rs; rewrite
as plain user docs. New top about: "maple — an always-fresh code-symbol graph that hands LLM coding
agents byte-sized, exact context bundles". Remove stale "UNVERIFIED / no Rust toolchain" comments in
main.rs header and Cargo.toml.

## F6 — legal + packaging (public-repo blockers)
- `LICENSE`: **Apache-2.0** (full standard Apache License 2.0 text; copyright "2026 Yonk Labs").
- `Cargo.toml [package]`: add `license = "Apache-2.0"`, `repository = "https://github.com/yonk-labs/maple"`,
  `readme = "README.md"`, `keywords = ["code-graph","llm","agents","tree-sitter","context"]`,
  `categories = ["command-line-utilities","development-tools"]`.

## F7 — CI
`.github/workflows/ci.yml`: on push+PR → checkout, stable Rust, `cargo build --release`,
`cargo test`, `cargo clippy --all-targets -- -D warnings` (so F8 must actually clear clippy),
ubuntu-latest. Cache cargo registry+target (standard actions/cache keys).

## F8 — clippy clean
`cargo clippy --fix --allow-dirty --all-targets`, then fix the remainder by hand (manual_div_ceil,
unnecessary_map_or, type_complexity via type aliases, too_many_arguments via an options struct for
`bundle()` if needed — pick minimal). End state: `cargo clippy --all-targets -- -D warnings` passes.

## F9 — hygiene
- `.gitignore`: add `skill-output/` and `.claude/`.
- Scrub absolute local paths from `spec/code-symbol-graph/waves/*.md` (replace with
  relative paths or `<repo>`); spec/ and spike/ otherwise SHIP AS-IS (deliberate: public dev history;
  bob/hector are public repos).
- Replace test email `t@t.com` with `test@example.com` in store.rs.

## F10 — README overhaul (the "excellent docs / dead simple" requirement)
Rewrite README.md. Required structure & content:
1. Title + one-line value prop + 3-bullet "why" (exact closure vs grep/RAG; token-budgeted bundles;
   always-fresh with no daemon). Keep it plain-English — the current para 2 assumes agent-tooling
   jargon; add one sentence defining the problem for a general Python/Rust dev.
2. **Install** (the missing bridge): `cargo install --path .` after clone (and note
   `cargo install --git https://github.com/yonk-labs/maple` once public); alternative
   `cargo build --release` + `./target/release/maple`. State Rust ≥ stable toolchain requirement.
3. **Quickstart**: copy-pasteable sequence against the reader's own repo, WITH sample output snippets
   (index line, enumerate JSON abridged, bundle --format prompt head) so success is self-verifiable.
4. **Command reference**: ALL subcommands (parse, index, status, closure, enumerate, bundle, exists,
   surface, impact, seed, gc, mcp) — one line + example each. Scope the target-spec grammar honestly:
   full grammar = closure/bundle/enumerate (post-F3); bare name/module = exists/surface.
5. **MCP integration**: how to wire into an MCP client, with a concrete config snippet, e.g.
   `{"mcpServers": {"maple": {"command": "maple", "args": ["mcp", "/path/to/repo"]}}}`, protocol
   version, the 3 tools and their arguments.
6. **JSON output reference**: brief field tables for bundle/closure/enumerate/impact (from the
   structs — target/callees/callers/report{token_count,over_budget,omitted,ambiguous,unresolved,
   unparsed_files}, etc.).
7. Day-0/new-projects + Operations sections: keep existing content, fold in tone-consistent.
8. **Numbers section** ("measured on real code"): god-file 33K tok → ~1.3K bundle (96%); SC-4 A/B
   local-model pass 3/3 bundle vs 0/2 whole-file (gemma) / 25× fewer tokens (qwen); warm query p95
   13–36ms; cold index ~2.2K files in ~8s. Link spec/ for the full evidence trail.
9. How it works (5 lines: tree-sitter parse → SQLite graph → delta self-heal → exact|ambiguous|
   unresolved labels, never guesses, never silently drops).

## Gates (all must pass before reporting)
- `cargo test` fully green (48+ incl. new F1/F2/F3 tests).
- `cargo clippy --all-targets -- -D warnings` clean.
- pg-raggraph: delete `.maple`, index, expect exactly 14484 edges / 4441 / 2029 / 8014 (F3/F4 must
  not change edge resolution; fq_name is display-only).
- Concurrent smoke: two `maple index` runs on a fixture launched simultaneously → final symbol count
  correct (not doubled).
- `maple index /nonexistent/path` → clear error, non-zero exit.
- `maple enumerate <repo> --symbol "pkg.mod.func"` works.
- README contains: install section, mcp config snippet, sample outputs, all 12 subcommands.

## Report back
Per-item status · gate outputs (abridged) · files changed + net lines · anything deferred and why.
