# WAVE 3 SPEC — Agent interfaces (T8 MCP · T9 diff impact · T10 lookup polish)

Repo `<repo>` (Rust; `source ~/.cargo/env`). Prereq: Wave 2 merged,
all tests green. May touch: `src/main.rs`, `src/store.rs`, `src/parser.rs` (T10 docstrings only), and
NEW file `src/mcp.rs`. Read existing code first. Constraints unchanged: D0, three labels only,
delta==rebuild, no heavyweight deps (K2) — **no MCP SDK crate; hand-roll JSON-RPC over stdio** (bob
does exactly this — crib the shape from `../bob/src/` MCP module).

## T8 — `maple mcp <repo>`: MCP stdio server
- JSON-RPC 2.0, line-delimited over stdin/stdout. Support: `initialize` (reply protocolVersion
  "2024-11-05", capabilities `{tools:{}}`, serverInfo name "maple"), `notifications/initialized`
  (ignore), `tools/list`, `tools/call`, graceful EOF exit. Unknown methods → JSON-RPC error -32601.
- Tools (thin wrappers over existing Store methods; each call runs `refresh()` first — A1):
  1. `enumerate` {symbol} → the Enumeration JSON.
  2. `closure` {symbol, depth?} → Closure JSON.
  3. `bundle` {symbol, budget?, max_callers?, format?("json"|"prompt")} → Bundle JSON or the W2.3
     prompt rendering as text content.
- `tools/call` result shape: `{content:[{type:"text", text:<payload>}]}`; errors → `isError:true`
  with the message. Symbol-not-found is a tool error, not a crash.
- Server holds ONE Store open on the repo path given at startup; each tools/call does the delta
  refresh (cheap; measured ms-scale) so long-lived sessions stay fresh (no daemon semantics beyond
  the conversation lifetime — this honors K1: the server lives only as long as the client).

## T9 — `maple impact <repo> --diff <rev>` (blast radius of a change)
- Run `git -C <repo> diff --unified=0 <rev>` (working tree vs rev; also accept `--staged`).
  Parse per-file hunk headers (`@@ -a,b +c,d @@`) → changed line ranges in the NEW file version.
  Non-git repo or bad rev → clear error, non-zero exit.
- `refresh()` first, then: changed symbols = symbols whose span intersects any changed range in that
  file (+ symbols in files deleted by the diff, reported as `deleted`).
- Output JSON: `{changed_symbols:[{fq_name,file,span,kind, caller_count, caller_files, callers:[{caller,file,line,resolution} … capped 20, "omitted" count]}], files_no_symbols:[…]}`.
- Purpose: bob pre-VERIFY scope check; hector follow-up-slice generation. Keep it read-only.

## T10 — Lookup & metadata polish
1. **FQ names**: bundle/closure/enumerate outputs report `fq_name` as `<module path>.<name>` where
   module path = file path minus `.py`, `/`→`.`, stripping a leading `src/` segment if present
   (e.g. `src/pg_raggraph/config.py::load` → `pg_raggraph.config.load`). Computed at query time —
   no schema change.
2. **Docstrings**: parser extracts a function/class's docstring (first statement = string literal),
   first line only, ≤120 chars → `Definition.docstring: Option<String>` → symbols column → bundle
   target + callee entries include it. (D4 always specced this; wire it through.)
3. **`module.func` spec form** (lookup_targets): exclude methods (`parent_class IS NOT NULL`) from
   the module-path branch so `pkg.y.foo` no longer returns `K.foo` from the same file.
4. **Unknown language**: `maple parse <file>` on a non-`.py` file → clear error ("unsupported
   language for <path>; v1 parses Python") with non-zero exit, instead of whatever happens now.

## Tests required
- T8: end-to-end MCP test — spawn the binary (`Command::new(env!("CARGO_BIN_EXE_maple"))`… or use
  `assert_cmd`-style manual spawn with std) feeding initialize→tools/list→tools/call(enumerate) over
  stdin against a tempdir fixture repo; assert tool list has 3 tools and the enumerate result parses.
  (If spawning the binary in tests is awkward, factor the dispatch into a testable
  `handle_request(&mut Store, Value) -> Value` and unit-test that; still add one spawn smoke test.)
- T9: fixture repo under `git init`; commit; edit a function; `impact --diff HEAD` lists exactly that
  symbol with its callers; edit outside any symbol (top-level constant) → `files_no_symbols`.
- T10: fq_name correctness (with and without `src/` prefix); docstring first-line in bundle; module
  form excludes methods (update the existing f13 test expectation — currently asserts 2 targets for
  `pkg.y.foo`, becomes 1); unknown-lang parse error.
- All prior tests stay green (only the documented f13 expectation changes).

## Measurement / smoke gate
- `cargo test` fully green.
- pg-raggraph: edge totals unchanged from Wave 2's accepted numbers (these features are read-side).
- Smoke: `maple impact` on pg-raggraph after touching one function (restore after);
  `printf` an initialize+tools/list into `maple mcp` and show the reply lines.

## Report back
Tests summary · smoke outputs (MCP reply head, impact JSON head) · deviations & why · files changed
+ net lines.
