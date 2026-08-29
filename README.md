# maple

**An always-fresh code-symbol graph that hands LLM coding agents byte-sized, exact context bundles.**

In plain terms: before you touch a function, you want to know every place that calls it and
everything it calls. Normally that means grepping (noisy, and easy to miss a caller hidden behind
an alias or a subclass) or reading unfamiliar code end to end. maple answers that exactly, from a
graph it parses out of your source and keeps in sync as files change — no server, no watcher, no
stale cache.

- **Exact, not fuzzy.** maple computes a real caller/callee closure from a parsed AST — the answer
  to "what calls this?" is a fact, not a guess. Grep burns your context window on exploration with
  no stopping condition; RAG retrieves by similarity with no guarantee it found every caller. A
  missed caller from either approach is a silent, invisible break.
- **Token-budgeted bundles.** `bundle` assembles target body + callee signatures + caller
  call-site snippets into one JSON payload sized for a small model's context window, and reports
  anything it had to omit, couldn't resolve, or found ambiguous — it never trims silently.
- **Always fresh, no daemon.** Every query re-parses only what changed since the last run (a
  git-aware delta, or a content hash walk) before answering. No background process, no file
  watcher, no hooks to install — just run the command.

The contract: **a caller is never silently dropped.** Every call-site becomes exactly one edge
labeled `exact`, `ambiguous` (candidates kept, flagged), or `unresolved` (dynamic/external, named).

Built as the "map" layer of a local-model pipeline —
[hector](https://github.com/yonk-labs/hector) plans slices, maple scopes them,
[bob](https://github.com/yonk-labs/bob) builds and verifies — but it's a standalone CLI with JSON
output; nothing here assumes that stack.

## Install

**The whole pipeline, one command** (maple + [bob](https://github.com/yonk-labs/bob) + [abe](https://github.com/yonk-labs/abe) + [hector](https://github.com/yonk-labs/hector) + [goose](https://github.com/block/goose), built, configured, and on your PATH — asks one question: where your model server is):

```bash
git clone https://github.com/yonk-labs/maple && bash maple/scripts/install-pipeline.sh
```

It installs Rust if needed, auto-detects your endpoint's model, pre-flights structured tool-calls
(and configures the goose toolshim fallback if your server can't), and never overwrites configs you
already have. Re-run any time to update everything.

**Just maple:**

Requires a stable Rust toolchain (any recent stable `rustc`/`cargo` — no nightly features used)
and, since v0.3.6, a C compiler (`cc`/`gcc`/`clang`) — Oracle PL/SQL support vendors a C scanner,
compiled via `build.rs`.

```bash
git clone https://github.com/yonk-labs/maple
cd maple
./install.sh            # builds release + installs to ~/.local/bin, checks prerequisites first
# or
cargo install --path .  # installs to ~/.cargo/bin
```

Once the repo is public, you can skip the clone:

```bash
cargo install --git https://github.com/yonk-labs/maple
```

Prefer not to install into `~/.cargo/bin`? Build and run the binary directly instead:

```bash
cargo build --release
./target/release/maple --help
```

The rest of this doc assumes `maple` is on your `PATH`; substitute `./target/release/maple` if not.

## Quickstart

Run this against any repo you already have checked out — no config file, no setup step beyond
this:

```bash
maple index /path/to/your/repo
```

```
indexed: 871 files, 6888 symbols, 5343 imports, 51765 edges (exact 11133, ambiguous 8563, unresolved 32069) -> /path/to/your/repo/.maple/graph.db
```

(your numbers will differ — this is a real run against a mid-size Python repo, shown so you can
tell success from failure; re-run periodically as the demo repo itself grows). Now ask it about a
real symbol in your repo:

```bash
maple enumerate /path/to/your/repo --symbol your_function_name
```

```json
{
  "symbol": "_observed_worker_count",
  "def_count": 2,
  "defs": [
    {
      "fq_name": ".worktrees.scale-remediation.src.pg_raggraph.config._observed_worker_count",
      "name": "_observed_worker_count",
      "file": ".worktrees/scale-remediation/src/pg_raggraph/config.py",
      "start_line": 55,
      "end_line": 67,
      "signature": "def _observed_worker_count() -> int:",
      "docstring": "Best-effort worker count from common deployment env vars."
    },
    {
      "fq_name": "pg_raggraph.config._observed_worker_count",
      "name": "_observed_worker_count",
      "file": "src/pg_raggraph/config.py",
      "start_line": 85,
      "end_line": 97,
      "signature": "def _observed_worker_count() -> int:",
      "docstring": "Best-effort worker count from common deployment env vars."
    }
  ],
  "caller_count": 2,
  "caller_file_count": 2,
  "exact": 2,
  "ambiguous": 0,
  "unresolved": 0,
  "unparsed_files_count": 15
}
```

(`def_count` is 2 here, not the 1 you'll usually see, because this particular checkout has a git
worktree checked in under `.worktrees/` — a byte-identical second copy of the file. Both are real,
distinct defs; each keeps its own callers, correctly. If your repo doesn't have that quirk, expect
1. `path.py::name` and bare `name` both suffix-match a file path, so either form still returns
both copies here — `path.py:LINE` is the one that actually pins a single def, since it also filters
by which span the line falls inside.)

And assemble a task-ready bundle for it:

```bash
maple bundle /path/to/your/repo --symbol your_function_name --format prompt
```

````
# Target: .worktrees.scale-remediation.src.pg_raggraph.config._observed_worker_count (.worktrees/scale-remediation/src/pg_raggraph/config.py:55-67)
```python
def _observed_worker_count() -> int:
    """Best-effort worker count from common deployment env vars."""
    for env_var in _WORKER_ENV_VARS:
        value = os.environ.get(env_var)
        ...
```
## Direct callees
- `get`  (ambiguous)
- `int`  (unresolved/external)
## Callers (2 total; 2 shown; tests first)
### .worktrees/scale-remediation/src/pg_raggraph/config.py:181 in model_post_init()
```python
        workers = _observed_worker_count()
        fleet_connections = self.pool_max * workers
        ...
```
### src/pg_raggraph/config.py:252 in model_post_init()
```python
        workers = _observed_worker_count()
        fleet_connections = self.pool_max * workers
        ...
```
````

That's the whole loop: index once, then query as often as you like — each query self-heals against
whatever changed on disk since the last index/query, so you never need to remember to re-index.

## Command reference

`<repo>` is a path to the repo root; state lives in `<repo>/.maple/graph.db`. The full target-spec
grammar (bare `name` · `path.py::name` · `path.py:LINE` · `module.path.name` / `Class.method`) is
honored by `closure`, `enumerate`, and `bundle`. `exists` and `surface` take a bare name/module
instead — see their entries below. `path.py` in any of these matches by path SUFFIX (so a short
relative path still finds a file nested deeper), which means it does NOT disambiguate two
identically-named files at different depths (e.g. a vendored copy or a checked-in git worktree) —
only `path.py:LINE` narrows to one, since it also filters by which def's span contains that line.

| Command | What it does |
|---|---|
| `maple parse <file>` | Parse one source file (any supported language), print its extracted defs/calls/imports as JSON. |
| `maple index <repo>` | Cold full index of a repo into `<repo>/.maple/graph.db`. `--sql-dialect postgres\|tsql\|plsql` for `.sql` files; `--sql-columns`/`--with-sql-cte`/`--orm-python` opt into column-level lineage (see [Column-level lineage](#column-level-lineage-wave-l3-opt-in)). |
| `maple status <repo>` | Print counts from an existing store without parsing anything. |
| `maple closure <repo> --symbol <spec>` | Depth-1 closure: target definition(s) plus direct callers and callees. |
| `maple enumerate <repo> --symbol <spec>` | "N callers across M files" plus an exact/ambiguous/unresolved breakdown. |
| `maple bundle <repo> --symbol <spec>` | Token-budgeted context bundle: target body, callee signatures, caller snippets. |
| `maple exists <repo> --name <name>` | Check whether a symbol name already exists before creating a new one. |
| `maple surface <repo> --module <path>` | API surface of a module: defs, classes with their methods, imports. |
| `maple impact <repo> --diff <rev>` | Blast radius of a diff: symbols it touches, plus their callers. |
| `maple seed <repo> --from <source>` | Warm-start a fresh worktree/clone from an existing index instead of a cold index. |
| `maple gc <repo> --yes` | Delete `<repo>/.maple` outright. |
| `maple mcp <repo>` | Serve the graph over MCP (JSON-RPC 2.0, stdio). |

Examples:

```bash
maple parse src/pkg/mod.py

maple index /path/to/repo

# .sql files need a dialect — postgres, tsql, or plsql (Oracle-only extensions never need it)
maple index /path/to/repo --sql-dialect postgres

# + column-level lineage: CREATE TABLE -> DML reads/writes, all opt-in, off by default
maple index /path/to/repo --sql-dialect tsql --sql-columns

# + resolve a WITH-clause CTE-scoped read against the real table/column it re-projects
maple index /path/to/repo --sql-dialect postgres --with-sql-cte

# + a SQLAlchemy model field resolves against the real schema column it maps to (cross-language)
maple index /path/to/repo --sql-dialect postgres --sql-columns --orm-python

maple status /path/to/repo

maple closure /path/to/repo --symbol "src/pkg/mod.py::embed" --depth 1

maple enumerate /path/to/repo --symbol embed
maple enumerate /path/to/repo --symbol "pkg.mod.Widget.render"   # Class.method form

maple bundle /path/to/repo --symbol embed --budget 16000 --max-callers 20
maple bundle /path/to/repo --symbol embed --format prompt        # task-ready markdown

maple exists /path/to/repo --name embed              # any existing defs/imports named this?
maple exists /path/to/repo --name embed --prefix     # widen to a prefix match

maple surface /path/to/repo --module pkg/mod.py      # or --module pkg.mod

maple impact /path/to/repo --diff HEAD~1             # or --staged

maple seed /path/to/worktree --from /path/to/main-checkout

maple gc /path/to/repo --yes

maple mcp /path/to/repo
```

## MCP integration

`maple mcp <repo>` speaks MCP (protocol `2024-11-05`) as a hand-rolled JSON-RPC 2.0 server over
stdio — no SDK dependency. Point any MCP-capable client at it:

```json
{
  "mcpServers": {
    "maple": {
      "command": "maple",
      "args": ["mcp", "/path/to/repo"]
    }
  }
}
```

maple also ships as a Claude Code plugin: `.claude-plugin/plugin.json` (name `maple`) plus
`.mcp.json` wire the same `maple mcp .` server into the manifest shape bob and abe use, and
`.claude-plugin/marketplace.json` makes it a one-command install like the others:

```
/plugin marketplace add yonk-labs/maple
/plugin install maple@yonk-labs
```

It exposes 3 tools, each refreshing the graph first so a long-lived session self-heals:

| Tool | Arguments |
|---|---|
| `enumerate` | `symbol` (required) |
| `closure` | `symbol` (required), `depth` (optional, default 1) |
| `bundle` | `symbol` (required), `budget`, `max_callers`, `format` (`"json"` or `"prompt"`) — all optional |

A symbol that doesn't resolve (or any other tool-level failure) comes back as an MCP tool error, not
a crash — the session stays alive.

All three tools also take an optional `continuation_id`. Omit it on the first call; the response
echoes one back that you can pass on later calls to thread a sequence of related queries into one
session — the same file-based, append-only continuation-memory pattern bob/hector/abe's MCP tools
use, with no daemon and no server-side state beyond a JSONL file (`$AGENT_THREAD_DIR`, default
`~/.cache/agent-thread`). An unrecognized `continuation_id` comes back as a tool error, same as an
unresolved symbol — not a crash.

## JSON output reference

Every query command prints one JSON object. The fields that show up across `bundle`/`closure`/
`enumerate`/`impact`:

**`bundle`**

| Field | Meaning |
|---|---|
| `target.fq_name` / `.file` / `.start_line`/`.end_line` / `.signature` / `.docstring` / `.body` | The resolved definition and its full source body. |
| `callees[]` | `name`, `resolution` (`exact`/`ambiguous`/`unresolved`), `file`, `signature`, `docstring` — direct calls made *by* the target. |
| `callers[]` | `caller`, `resolution`, `is_test`, `call_site.{file,line,snippet}` — direct calls *to* the target, capped at `--max-callers` (tests first, never evicted by the cap). |
| `report.token_count` / `.budget` / `.over_budget` | Approximate token size vs. the requested budget — a signal, never a silent trim. |
| `report.caller_count` / `.callers_included` / `.test_caller_count` | Full fan-in vs. how many caller snippets made it into this bundle. |
| `report.omitted[]` | `file:line` of callers cut by the cap — reported, never silently dropped. |
| `report.ambiguous[]` / `.unresolved[]` | Callee names that couldn't be pinned to one definition, or that resolve outside the repo. |
| `report.unparsed_files[]` / `.unparsed_files_count` | Files with holes in the graph (capped list; count is always the true total). |
| `meta.depth` / `.tokenizer` / `.ambiguous_target` | Query params echoed back, plus whether the target symbol itself was ambiguous. |

**`closure`**

| Field | Meaning |
|---|---|
| `symbol` / `depth` | The resolved bare name and closure depth (v1 is always depth 1). |
| `targets[]` | Matching definition(s): `fq_name`, `file`, `start_line`/`end_line`, `signature`, `docstring`. |
| `callers[]` | `caller`, `file`, `line`, `resolution` — every call-site resolving to this symbol. |
| `callees[]` | `name`, `resolution`, `file`, `start_line`, `signature`, `docstring` — deduped calls the target makes. |

**`enumerate`**

| Field | Meaning |
|---|---|
| `symbol` / `def_count` / `defs[]` | The resolved bare name, how many definitions match, and the definitions themselves. |
| `caller_count` / `caller_file_count` | Total calls to this symbol, and how many distinct files they come from. |
| `exact` / `ambiguous` / `unresolved` | The same total, split by resolution label. |
| `unparsed_files_count` | Repo-wide count of files with parse holes (independent of the queried symbol). |

**`impact`**

| Field | Meaning |
|---|---|
| `changed_symbols[]` | `fq_name`, `file`, `start_line`/`end_line`, `kind`, `status` (`"changed"` or `"deleted"`). |
| `changed_symbols[].caller_count` / `.caller_files` / `.callers[]` / `.callers_omitted` | Blast radius: full fan-in, capped caller list (20), and how many were cut. |
| `files_no_symbols[]` | Files the diff touched where the edit landed outside any def/class span. |

Wave L3: `kind` on `impact`'s `changed_symbols[]` now also takes `table`/`column` (alongside
`function`/`class`) — a changed `CREATE TABLE` column's callers include every DML read/write, CTE
reference, and (with `--orm-python`) ORM model field that maps to it, across languages. `kind` is
the ONLY place a symbol's kind is currently exposed in JSON — `bundle`/`closure`/`enumerate`'s
`defs[]`/`targets[]` entries don't carry it. Likewise, whether a given caller *reads* or *writes*
the column (or is an `orm-map`) isn't its own JSON field on any command yet — the graph tracks it
internally (`read`/`write`/`orm-map` are real `call_kind` values in the store), but today you tell
them apart by looking at `file`/`line` (an ORM mapping's caller is a `.py` file; a DML write's
`caller` is the enclosing procedure).

## Day 0 / new projects

A brand-new repo (even a single near-empty `.py` file) works exactly like a mature one:

```bash
maple index /path/to/new-repo   # once, at repo creation (or right after the first real commit)
```

Every query after that self-heals to current file state first — no daemon, no file-watcher, no
git hooks to install. Add a file, add a symbol, then call `enumerate`/`closure`/`bundle` again: the
delta refresh picks it up before answering. A brand-new symbol with zero callers is a valid, honest
answer (`caller_count: 0`), not an error.

**Worktrees:** a fresh `git worktree` starts with no index. Instead of a cold `maple index` (a full
re-parse), warm-start it from the parent checkout:

```bash
maple seed /path/to/worktree --from /path/to/main-checkout
```

This copies the existing graph, then runs one delta refresh against the worktree's actual files —
O(branch-diff) instead of O(repo). Refuses to overwrite an existing target index unless `--force`.

**Writing new code:** before adding a symbol, check it isn't already there:

```bash
maple exists  /path/to/repo --name embed          # any existing defs/imports named this?
maple surface /path/to/repo --module pkg/mod.py   # the module's current API surface
```

Empty `defs` from `exists` means it's safe to create; `surface` shows what a module already
exports (module-level defs, plus classes with their methods nested) before you extend it.

**Parse-failure warnings** (`status`, `bundle`'s report, `--format prompt`, `enumerate`'s
`unparsed_files_count`) mean the graph has holes for those files — treat their absence from query
results as "unknown," not "no callers." tree-sitter is error-tolerant, so this fires for files that
are unreadable (permission-denied) or that parse but yield zero defs/calls/imports for non-empty
content (a real source file usually wouldn't; a data-only, misidentified, or declarations-only
file might).

## Operations

State lives in `<repo>/.maple/graph.db` (SQLite). A few things worth knowing before running maple
against a real repo:

- **Schema changes auto-reset.** Every db tracks a schema version (`PRAGMA user_version`). If a
  future maple build's schema doesn't match what's on disk, `open()` drops and recreates the tables
  automatically — the next `index`/query rebuilds the graph from disk. You never need to manually
  `rm -rf .maple` after a maple upgrade.
- **Concurrent writers wait, not error.** Cold indexing clears and rewrites the graph inside a
  single transaction with a 10s busy timeout, so two `index` runs racing on the same repo serialize
  at the database instead of doubling the graph or erroring out — and a process killed mid-index
  (e.g. `Ctrl-C`) rolls back to the previous graph instead of leaving it empty.
- **`maple gc <repo> --yes`** deletes `<repo>/.maple` outright (no confirmation without `--yes`).
  Use it to force a fully clean re-index, or to reclaim disk space for a repo you're done with.
- **`maple seed <repo> --from <source>`** warm-starts a new worktree/clone from an existing index
  instead of a cold `index` — see [Worktrees](#day-0--new-projects) above.

## Numbers (measured on real code)

- **God-file bundle:** a 33K-token file (over most small-model context windows) → a ~1.3K-token
  `bundle` for the one function that needed editing — a ~96% reduction, and the difference between
  "doesn't fit" and "fits."
- **Local-model A/B (SC-4):** a real bug fixed against that same 33K-token god-file, verified by the
  repo's own pytest suite. With the bundle as context: 3/3 pass (qwen2.5-coder), 2/3 pass (gemma).
  With the whole file truncated to the model's window instead: 0/2 pass (gemma) — whole-file context
  can't even represent the fix; the bundle is ~25× fewer tokens for the same task.
  Small-n baseline, not a final target — see `spec/` for the full write-up.
- **Warm query latency:** p95 13–36ms per query (release build, delta self-heal included; the
  higher end includes a git shell-out for the git-aware fast path).
  **Cold index:** ~2.2K files in ~8s (parallel parse via `rayon`; SQLite writes stay single-threaded).

The full evidence trail — methodology, fixtures, and every wave's before/after numbers — lives in
[`spec/code-symbol-graph/`](spec/code-symbol-graph/).

## How it works

1. **Parse.** tree-sitter turns each source file (9 languages + 3 SQL dialects — see the table
   below) into defs, call-sites, imports, and aliases.
2. **Store.** Symbols and calls land in a SQLite graph (`<repo>/.maple/graph.db`).
3. **Resolve.** Every call-site becomes exactly one edge, deterministically labeled `exact`,
   `ambiguous`, or `unresolved` — never a similarity score, never guessed.
4. **Self-heal.** Every query re-parses only what changed (git-aware delta, or a hash walk) before
   answering — the graph is never stale, and there's no daemon to keep alive.
5. **Never drop, never trim silently.** A caller you can't resolve is labeled and kept, not
   discarded; a bundle over budget is flagged `over_budget`, not silently truncated.

## Languages

v1.2: 9 programming languages plus 3 SQL dialects at the **universal tier** — defs (with parent
class/impl-type/receiver containers), call-sites split func-vs-method by syntax, imports and
aliases, name-based resolution **scoped to the caller's language** (a `.rs` call never matches a
`.java` def; cross-language calls like FFI are honest `unresolved`). Only Python additionally has
the **exact resolver** — the type-aware layer that binds `self.foo()` / `x = C(); x.foo()` /
annotated params / one-hop inheritance / import-aware bare calls deterministically. Receiver hints
outside Python exist only where the syntax hands them over for free; nothing is inferred.

| Language | Extensions | Defs + containers | func/method calls | Imports/aliases | Receiver hints | Docstrings |
|---|---|---|---|---|---|---|
| Python | `.py` | ✓ classes | ✓ | ✓ `import`/`from`/`as` | ✓ full exact resolver (S2/T1–T4) | ✓ |
| Rust | `.rs` | ✓ struct/enum/trait + `impl` blocks | ✓ (`X::y` counts as method) | ✓ `use`, `use .. as` | `self.foo()` in `impl T` → T | ✓ `///` |
| C | `.c` `.h` | ✓ (no classes) | all `func` (C has no methods) | `#include` raw only | — | — |
| C++ | `.cpp` `.cc` `.hpp` `.hh` | ✓ class/struct + out-of-line `X::y` defs | ✓ | `#include` raw only | — | — |
| C# | `.cs` | ✓ class/interface/struct/record | ✓ | ✓ `using`, `using X = Y` | — | ✓ `///` / `/** */` |
| Java | `.java` | ✓ class/interface/enum/record | ✓ | ✓ imports (last segment) | — | ✓ `/** */` |
| JavaScript | `.js` `.jsx` `.mjs` `.cjs` | ✓ classes + `const x = () =>` arrows | ✓ | ✓ `import {a as b}`, defaults | — | ✓ `/** */` |
| TypeScript | `.ts` `.tsx` | ✓ (TS + TSX grammars, one language) | ✓ | ✓ `import {a as b}`, defaults | — | ✓ `/** */` |
| Go | `.go` | ✓ named types + method receivers | ✓ | ✓ import aliases | `w.foo()` on the receiver ident → type | — |
| PostgreSQL | `.sql`* | ✓ functions/procedures (schema as container); plpgsql + `LANGUAGE sql` dollar-quoted bodies re-parsed for calls | ✓ (`schema.fn()` → method) | — | — | — |
| T-SQL | `.sql`* | ✓ procedures/functions/triggers (schema as container); `GO` separators handled | ✓ (`EXEC`, `dbo.proc()` → method) | — | — | — |
| Oracle PL/SQL | `.sql`* `.pks` `.pkb` `.prc` `.fnc` `.trg` `.pls` | ✓ packages/object types as class containers; body members carry the package (spec decls emit no def) | ✓ (`pkg.proc()` → method) | — | `pkg.proc()` → pkg (the qualifier is free) | — |

\* `.sql` alone can't name its dialect and maple never guesses: run
`maple index <repo> --sql-dialect=postgres|tsql|plsql` once — the setting persists in the store, so
refresh and queries inherit it. Without it, `.sql` files are skipped. Oracle-only extensions
(`.pks` etc.) never need the flag. SQL identifiers fold case, so all SQL symbols are stored
lowercase; common builtins (`count`, `nvl`, `getdate`, ...) are skipped at extraction so
query-embedded calls don't flood the graph. Dynamic SQL (`EXECUTE IMMEDIATE`, `sp_executesql`) is
never extracted — honest absence over guessing.

Shallow by design: the universal tier extracts what the syntax states and resolves by name within
the language — it over-reports `ambiguous`/`unresolved` rather than guess (C++ especially). T-SQL
grammar gaps (legacy unparenthesized parameter lists, `OPENJSON ... WITH`, multi-statement
table-valued functions) cost some extraction — affected files are flagged in `parse_failures`, so
the graph always reports its holes (measured ~87% file-level proc recall on Microsoft's
WideWorldImporters).

## Column-level lineage (Wave L3, opt-in)

Beyond the universal tier, maple can track "what reads/writes this column, all the way from schema
to application code" — every layer is opt-in and off by default (new, unvalidated behavior shipped
dark until proven out on real corpora). Each flag persists in the store the same way
`--sql-dialect` does, so `refresh` inherits it without repeating the flag.

- **`--sql-columns`** — adds two new symbol kinds: `table` and `column`, extracted from
  `CREATE TABLE` (all 3 SQL dialects). `--with-sql-cte` and `--orm-python` both imply this: neither
  has anything real to resolve against without it.
- **`--sql-columns` alone** additionally extracts DML column references (`SELECT`/`WHERE`/`JOIN`
  reads, `INSERT`/`UPDATE`/`column :=` writes) inside procedure/function/trigger bodies and
  top-level scripts, as new call kinds `read`/`write`. An unqualified column resolves via the
  FROM/JOIN table set: exactly one candidate table defines it → `exact`; 2+ → `ambiguous` (never
  guessed); none → `unresolved` (could be a CTE, temp table, or an unindexed table).
- **`--with-sql-cte`** (T-SQL, Postgres, PL/SQL) — resolves a `WITH x AS (...)` CTE-scoped read
  against the REAL table/column it re-projects, instead of leaving it unresolved against a
  `parent_class` that's just a CTE name with no schema behind it. Only a CTE column that's a
  direct, untransformed passthrough of a real column gets resolved (`col`, `t.col`, `t.col AS
  alias`); a window function, aggregate, expression, or `*` — or a CTE body ambiguous between two
  underlying tables — is left honestly unresolved, never guessed.
- **`--orm-python`** (SQLAlchemy only; Django deferred, no real corpus to validate it against yet)
  — a declarative model's field → column mapping as a new call kind, `orm-map`: `class User(Base):
  __tablename__ = "users"; id = Column(...)` (classic) and `id: Mapped[int] = mapped_column(...)`
  (SQLAlchemy 2.0) both extract, using an explicit `Column("real_name", ...)` /
  `name=`/`db_column=` override when present, else the attribute's own name. This is the ONE
  deliberate cross-language edge in the whole graph (caller = a Python `class`, callee = a SQL
  `column`) — every other resolution path stays strictly same-language by design.
  **Scope boundary, not an implementation detail:** raw embedded SQL strings
  (`cursor.execute(f"...")`, template-literal SQL) are explicitly OUT OF SCOPE — string
  interpolation makes the real runtime SQL unknowable at parse time, and guessing here would break
  the never-guess contract the whole trust model depends on. `relationship()`/association fields
  and mixin-inherited columns are not walked (v1 scope: a model's own direct `Column`/
  `mapped_column` fields only).

## Status

Spec, plan, and evidence live in `spec/code-symbol-graph/`.
