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

Requires a stable Rust toolchain (any recent stable `rustc`/`cargo` — no nightly features used).

```bash
git clone https://github.com/yonk-labs/maple
cd maple
cargo install --path .
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
indexed: 309 files, 2244 symbols, 1786 imports, 14484 edges (exact 4441, ambiguous 2029, unresolved 8014) -> /path/to/your/repo/.maple/graph.db
```

(your numbers will differ — this is a real run against a mid-size Python repo, shown so you can
tell success from failure). Now ask it about a real symbol in your repo:

```bash
maple enumerate /path/to/your/repo --symbol your_function_name
```

```json
{
  "symbol": "_observed_worker_count",
  "def_count": 1,
  "defs": [
    {
      "fq_name": "pg_raggraph.config._observed_worker_count",
      "name": "_observed_worker_count",
      "file": "src/pg_raggraph/config.py",
      "start_line": 56,
      "end_line": 68,
      "signature": "def _observed_worker_count() -> int:",
      "docstring": "Best-effort worker count from common deployment env vars."
    }
  ],
  "caller_count": 1,
  "caller_file_count": 1,
  "exact": 1,
  "ambiguous": 0,
  "unresolved": 0,
  "unparsed_files_count": 5
}
```

And assemble a task-ready bundle for it:

```bash
maple bundle /path/to/your/repo --symbol your_function_name --format prompt
```

````
# Target: pg_raggraph.config._observed_worker_count (src/pg_raggraph/config.py:56-68)
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
## Callers (1 total; 1 shown; tests first)
### src/pg_raggraph/config.py:202 in model_post_init()
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
instead — see their entries below.

| Command | What it does |
|---|---|
| `maple parse <file.py>` | Parse one Python file, print its extracted defs/calls/imports as JSON. |
| `maple index <repo>` | Cold full index of a repo into `<repo>/.maple/graph.db`. |
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

It exposes 3 tools, each refreshing the graph first so a long-lived session self-heals:

| Tool | Arguments |
|---|---|
| `enumerate` | `symbol` (required) |
| `closure` | `symbol` (required), `depth` (optional, default 1) |
| `bundle` | `symbol` (required), `budget`, `max_callers`, `format` (`"json"` or `"prompt"`) — all optional |

A symbol that doesn't resolve (or any other tool-level failure) comes back as an MCP tool error, not
a crash — the session stays alive.

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
content (a real Python file wouldn't; a data-only or misidentified file might).

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

1. **Parse.** tree-sitter turns each `.py` file into defs, call-sites, imports, and aliases.
2. **Store.** Symbols and calls land in a SQLite graph (`<repo>/.maple/graph.db`).
3. **Resolve.** Every call-site becomes exactly one edge, deterministically labeled `exact`,
   `ambiguous`, or `unresolved` — never a similarity score, never guessed.
4. **Self-heal.** Every query re-parses only what changed (git-aware delta, or a hash walk) before
   answering — the graph is never stale, and there's no daemon to keep alive.
5. **Never drop, never trim silently.** A caller you can't resolve is labeled and kept, not
   discarded; a bundle over budget is flagged `over_budget`, not silently truncated.

## Status

v1: Python (universal tree-sitter tier + an exact resolver that binds `self.foo()` /
`x = C(); x.foo()` / bare-call scoping deterministically — it narrows, never guesses).
Spec, plan, and evidence live in `spec/code-symbol-graph/`.
