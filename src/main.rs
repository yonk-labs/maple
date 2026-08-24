//! maple CLI — parses source (9 languages + 3 SQL dialects, see `parser::LANGS` and
//! `parser::SqlDialect`) into an exact caller/callee graph
//! and serves it as JSON: `parse` a single file, `index`/`refresh` a repo into `.maple/graph.db`,
//! then query it with `closure`, `enumerate`, `bundle`, `exists`, `surface`, `impact`, or serve it
//! live over MCP.

mod langs;
mod langs_sql;
mod mcp;
mod parser;
mod store;
mod thread;

use clap::{Parser as ClapParser, Subcommand};
use std::fs;
use std::path::Path;

#[derive(ClapParser)]
#[command(
    name = "maple",
    version,
    about = "maple — an always-fresh code-symbol graph that hands LLM coding agents byte-sized, exact context bundles"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Parse one source file (any registered language) and print extracted symbols
    /// (defs/calls/imports) as JSON.
    Parse {
        path: String,
        /// Dialect for `.sql` files: postgres, tsql, or plsql (L2.1 — the extension alone can't
        /// name the dialect). Oracle-only extensions (.pks/.pkb/.prc/.fnc/.trg/.pls) never need it.
        #[arg(long)]
        sql_dialect: Option<String>,
        /// L3 (Wave L3, Phase A) — also extract table/column defs from CREATE TABLE. Off by
        /// default: new, unvalidated behavior, shipped dark until proven out on real corpora.
        #[arg(long, default_value_t = false)]
        sql_columns: bool,
        /// L3.4 (CTE lineage, pilot: T-SQL only) — resolve a CTE-scoped column read against the
        /// real table/column it re-projects, instead of leaving it unresolved. Off by default;
        /// implies --sql-columns (a CTE read resolves against real column defs, which need it on).
        #[arg(long, default_value_t = false)]
        with_sql_cte: bool,
    },
    /// Cold full index of a repo into <repo>/.maple/graph.db.
    Index {
        repo: String,
        /// Dialect for this repo's `.sql` files: postgres, tsql, or plsql. Persisted in the store,
        /// so refresh and queries inherit it; without it `.sql` files are not indexed.
        #[arg(long)]
        sql_dialect: Option<String>,
        /// L3 (Wave L3, Phase A) — also extract table/column defs from CREATE TABLE. Off by
        /// default: new, unvalidated behavior, shipped dark until proven out on real corpora.
        /// Persisted in the store, so refresh inherits it the same way --sql-dialect does.
        #[arg(long, default_value_t = false)]
        sql_columns: bool,
        /// L3.4 (CTE lineage, pilot: T-SQL only) — resolve a CTE-scoped column read against the
        /// real table/column it re-projects, instead of leaving it unresolved. Off by default,
        /// persisted like --sql-columns; implies --sql-columns (a CTE read resolves against real
        /// column defs, which need it on).
        #[arg(long, default_value_t = false)]
        with_sql_cte: bool,
    },
    /// Read counts from an existing store without parsing (no reindex, just reports what's there).
    Status { repo: String },
    /// Depth-1 closure of a symbol: target definition(s) plus its direct callers and callees. JSON.
    Closure {
        repo: String,
        #[arg(long)]
        symbol: String,
        #[arg(long, default_value_t = 1)]
        depth: i64,
    },
    /// Count callers and definitions for a symbol: N callers across M files, plus an
    /// exact/ambiguous/unresolved breakdown. JSON.
    Enumerate {
        repo: String,
        #[arg(long)]
        symbol: String,
    },
    /// Check whether a symbol name already exists (definitions + import matches) before creating a
    /// new one. Empty defs means it's safe to create. Exact match only, never fuzzy. JSON.
    Exists {
        repo: String,
        #[arg(long)]
        name: String,
        /// Widen to a case-sensitive prefix match (still deterministic, never fuzzy).
        #[arg(long, default_value_t = false)]
        prefix: bool,
    },
    /// Show a module's API surface (`--module path.py` or `pkg.mod`): module-level defs and
    /// classes with their methods, plus raw imports and an unparsed flag. JSON.
    Surface {
        repo: String,
        #[arg(long)]
        module: String,
    },
    /// Warm-start `<repo>`'s index by copying `--from <source-repo>`'s graph, then running a delta
    /// refresh against `<repo>`'s actual files — much faster than a cold index for a fresh
    /// worktree or clone. Refuses to overwrite an existing `<repo>` index unless `--force`.
    Seed {
        repo: String,
        #[arg(long)]
        from: String,
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Assemble a token-budgeted context bundle for a symbol: target body, callee signatures, and
    /// caller call-site snippets. `--format json` (default) or `prompt` for task-ready markdown.
    Bundle {
        repo: String,
        #[arg(long)]
        symbol: String,
        #[arg(long, default_value_t = 16000)]
        budget: i64,
        #[arg(long, default_value_t = 20)]
        max_callers: usize,
        #[arg(long, default_value_t = 1)]
        depth: i64,
        /// Snippet radius: lines before/after a non-test call-site.
        #[arg(long, default_value_t = 3)]
        snippet_radius: i64,
        /// Token-counting implementation. Only "approx" exists today; the flag stays stable as
        /// more are added.
        #[arg(long, default_value = "approx")]
        tokenizer: String,
        /// "json" (default) or "prompt" (task-ready markdown).
        #[arg(long, default_value = "json")]
        format: String,
    },
    /// Blast radius of a change: symbols touched by a diff, plus their callers. Read-only.
    Impact {
        repo: String,
        /// Diff working tree against this rev (`git diff --unified=0 <rev>`).
        #[arg(long)]
        diff: Option<String>,
        /// Diff staged changes against HEAD instead of `--diff <rev>`.
        #[arg(long, default_value_t = false)]
        staged: bool,
    },
    /// Serve the graph over MCP (JSON-RPC 2.0, stdio) for editor/agent integration.
    Mcp { repo: String },
    /// Delete `<repo>/.maple` (the index and its state). Requires `--yes` — there is no
    /// destructive default. The next `index` or query rebuilds it from disk.
    Gc {
        repo: String,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
}

/// T14 — `maple gc`: returns the message to print rather than printing directly, so it's testable
/// without capturing stdout.
fn gc_maple_dir(repo: &Path, yes: bool) -> anyhow::Result<String> {
    let dir = repo.join(".maple");
    if !yes {
        anyhow::bail!("maple gc: pass --yes to confirm deleting {}", dir.display());
    }
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
        Ok(format!("removed {}", dir.display()))
    } else {
        Ok(format!("nothing to remove: {} does not exist", dir.display()))
    }
}

/// T15 — the parse-failure warning line shared by `status` and `--format prompt`, factored out for
/// testability. `count` and `files` are separate because `bundle`'s report caps `files` at 10 while
/// `count` stays the true total (N1: the cap is never mistaken for the full picture). `None` when
/// there's nothing to report — the line is only ever a signal, never noise.
fn unparsed_status_line(count: usize, files: &[String]) -> Option<String> {
    if count == 0 {
        None
    } else {
        Some(format!("{count} files unparsed (graph incomplete): {}", files.join(", ")))
    }
}

/// Markdown fence language tag from a file's extension — display-only, for `--format prompt`.
/// Unknown extensions get a bare fence.
fn fence_lang(file: &str) -> &'static str {
    match file.rsplit_once('.').map(|(_, e)| e) {
        Some("py") => "python",
        Some("rs") => "rust",
        Some("c" | "h") => "c",
        Some("cpp" | "cc" | "hpp" | "hh") => "cpp",
        Some("cs") => "csharp",
        Some("java") => "java",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("ts" | "tsx") => "typescript",
        Some("go") => "go",
        Some("sql" | "pks" | "pkb" | "prc" | "fnc" | "trg" | "pls") => "sql",
        _ => "",
    }
}

/// W2.3 — render the existing `Bundle` struct as task-ready markdown (`--format prompt`). No
/// separate assembly path: every field here already exists on `Bundle` for the `json` format.
fn render_prompt(b: &store::Bundle) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Target: {} ({}:{}-{})\n```{}\n{}\n```\n",
        b.target.fq_name,
        b.target.file,
        b.target.start_line,
        b.target.end_line,
        fence_lang(&b.target.file),
        b.target.body
    ));

    out.push_str("## Direct callees\n");
    for c in &b.callees {
        let line = match c.resolution.as_str() {
            "exact" => format!(
                "- `{}`  ({})",
                c.signature.as_deref().unwrap_or(&c.name),
                c.file.as_deref().unwrap_or("?")
            ),
            "unresolved" => format!("- `{}`  (unresolved/external)", c.name),
            _ => format!("- `{}`  (ambiguous)", c.name),
        };
        out.push_str(&line);
        out.push('\n');
    }

    out.push_str(&format!(
        "## Callers ({} total; {} shown; tests first)\n",
        b.report.caller_count, b.report.callers_included
    ));
    for c in &b.callers {
        let flag = if c.is_test { "   [TEST]" } else { "" };
        out.push_str(&format!(
            "### {}:{} in {}(){}\n```{}\n{}\n```\n",
            c.call_site.file,
            c.call_site.line,
            c.caller,
            flag,
            fence_lang(&c.call_site.file),
            c.call_site.snippet
        ));
    }

    out.push_str("## Report\n");
    out.push_str(&format!(
        "tokens\u{2248}{} budget={} over_budget={}; omitted: {} callers ({}); ambiguous: [{}]; unresolved: [{}]\n",
        b.report.token_count,
        b.report.budget,
        b.report.over_budget,
        b.report.omitted.len(),
        b.report.omitted.join(" "),
        b.report.ambiguous.join(", "),
        b.report.unresolved.join(", "),
    ));
    if let Some(line) = unparsed_status_line(b.report.unparsed_files_count, &b.report.unparsed_files) {
        out.push_str(&format!("! {line}\n"));
    }
    out
}

fn refresh_note(s: &mut store::Store) -> anyhow::Result<()> {
    let st = s.refresh()?;
    if st.changed + st.deleted > 0 {
        eprintln!("refreshed: {} changed, {} deleted, {} edges relabeled", st.changed, st.deleted, st.relabeled);
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Parse { path, sql_dialect, sql_columns, with_sql_cte } => {
            let dialect = sql_dialect.as_deref().map(parser::SqlDialect::parse).transpose()?;
            let lang = require_supported(&path, dialect)?;
            let src = fs::read_to_string(&path)?;
            let mut parsed = (lang.parse)(&src)?;
            if !(sql_columns || with_sql_cte) {
                parsed.defs.retain(|d| d.kind != "table" && d.kind != "column");
            }
            if with_sql_cte {
                store::apply_cte_columns(&mut parsed);
            }
            println!("{}", serde_json::to_string_pretty(&parsed)?);
        }
        Cmd::Index { repo, sql_dialect, sql_columns, with_sql_cte } => {
            let root = Path::new(&repo);
            let mut s = store::Store::open(root)?;
            if let Some(d) = sql_dialect.as_deref() {
                s.set_sql_dialect(parser::SqlDialect::parse(d)?)?;
            }
            // Only ever turns ON here, mirroring --sql-dialect's "explicit value wins, omission
            // preserves whatever was persisted" behavior — a bare bool flag can't distinguish
            // "not passed" from "explicitly false" the way sql_dialect's Option<String> can, so
            // there's currently no CLI path back to off once a repo has opted in; delete .maple
            // and reindex if that's ever needed.
            if sql_columns || with_sql_cte {
                s.set_sql_columns(true)?;
            }
            if with_sql_cte {
                s.set_sql_cte(true)?;
            }
            let st = s.index_repo(root)?;
            println!(
                "indexed: {} files, {} symbols, {} imports, {} edges (exact {}, ambiguous {}, unresolved {}) -> {}",
                st.files, st.symbols, st.imports, st.edges, st.exact, st.ambiguous, st.unresolved,
                root.join(".maple/graph.db").display()
            );
        }
        Cmd::Status { repo } => {
            let root = Path::new(&repo);
            let s = store::Store::open(root)?;
            let (f, sy, im) = s.counts()?;
            let (e, ex, am, un) = s.edge_stats()?;
            println!("store: {f} files, {sy} symbols, {im} imports, {e} edges (exact {ex}, ambiguous {am}, unresolved {un}) — read from db, no parse");
            let unparsed = s.parse_failures()?;
            if let Some(line) = unparsed_status_line(unparsed.len(), &unparsed) {
                println!("{line}");
            }
        }
        // queries self-heal to current truth first (S1.6 refresh = the A1 freshness guarantee)
        Cmd::Closure { repo, symbol, depth } => {
            let mut s = store::Store::open(Path::new(&repo))?;
            refresh_note(&mut s)?;
            println!("{}", serde_json::to_string_pretty(&s.closure(&symbol, depth)?)?);
        }
        Cmd::Enumerate { repo, symbol } => {
            let mut s = store::Store::open(Path::new(&repo))?;
            refresh_note(&mut s)?;
            println!("{}", serde_json::to_string_pretty(&s.enumerate(&symbol)?)?);
        }
        Cmd::Exists { repo, name, prefix } => {
            let mut s = store::Store::open(Path::new(&repo))?;
            refresh_note(&mut s)?;
            println!("{}", serde_json::to_string_pretty(&s.exists(&name, prefix)?)?);
        }
        Cmd::Surface { repo, module } => {
            let mut s = store::Store::open(Path::new(&repo))?;
            refresh_note(&mut s)?;
            println!("{}", serde_json::to_string_pretty(&s.surface(&module)?)?);
        }
        Cmd::Seed { repo, from, force } => {
            let stats = store::seed(Path::new(&repo), Path::new(&from), force)?;
            println!(
                "seeded {repo} from {from}: {} changed, {} deleted, {} edges relabeled",
                stats.changed, stats.deleted, stats.relabeled
            );
        }
        Cmd::Bundle { repo, symbol, budget, max_callers, depth, snippet_radius, tokenizer, format } => {
            if tokenizer != "approx" {
                anyhow::bail!("unknown --tokenizer {tokenizer:?} (only \"approx\" is supported)");
            }
            let mut s = store::Store::open(Path::new(&repo))?;
            refresh_note(&mut s)?;
            let bundle = s.bundle(&symbol, budget, max_callers, depth, snippet_radius, &store::ApproxTokenizer)?;
            match format.as_str() {
                "json" => println!("{}", serde_json::to_string_pretty(&bundle)?),
                "prompt" => print!("{}", render_prompt(&bundle)),
                other => anyhow::bail!("unknown --format {other:?} (expected \"json\" or \"prompt\")"),
            }
        }
        Cmd::Impact { repo, diff, staged } => {
            let diff_text = store::run_git_diff(&repo, diff.as_deref(), staged)?;
            let (changed, deleted) = store::parse_diff(&diff_text);
            let mut s = store::Store::open(Path::new(&repo))?;
            println!("{}", serde_json::to_string_pretty(&s.impact(&changed, &deleted)?)?);
        }
        Cmd::Mcp { repo } => {
            mcp::serve(Path::new(&repo))?;
        }
        Cmd::Gc { repo, yes } => {
            println!("{}", gc_maple_dir(Path::new(&repo), yes)?);
        }
    }
    Ok(())
}

/// T10.4/L1.1 — an unregistered extension is a clear config error, not silent garbage.
/// L2.1: a `.sql` file without a dialect names the flag instead of pretending to be unsupported.
fn require_supported(path: &str, sql: Option<parser::SqlDialect>) -> anyhow::Result<&'static parser::LangSpec> {
    parser::lang_for_path(Path::new(path), sql).ok_or_else(|| {
        if path.rsplit_once('.').is_some_and(|(_, e)| e == "sql") {
            anyhow::anyhow!(".sql needs a dialect: pass --sql-dialect=postgres|tsql|plsql")
        } else {
            anyhow::anyhow!("unsupported language for {path}; supported: {}", parser::supported_extensions_list())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W2.3 — `--format prompt` renders the existing `Bundle` as task-ready markdown: target fence,
    /// `[TEST]`-flagged callers, and the report line (omissions/ambiguity/unresolved never optional — N1).
    #[test]
    fn w23_render_prompt_contains_required_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.py"), "def target():\n    return missing()\n").unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("tests/test_lib.py"), "def test_it():\n    target()\n    assert True\n").unwrap();
        let mut s = store::Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        let bundle = s.bundle("target", 16000, 20, 1, 3, &store::ApproxTokenizer).unwrap();
        let out = render_prompt(&bundle);

        // T10.1: fq_name is now genuinely qualified (module path + name), not the bare symbol name.
        assert!(out.contains("# Target: lib.target ("), "target header");
        assert!(out.contains("```python"), "fenced code block");
        assert!(out.contains("[TEST]"), "test caller flagged");
        assert!(out.contains("## Report"), "report section always rendered (N1)");
        assert!(out.contains("tokens\u{2248}"), "token count included");
        assert!(out.contains("unresolved: [missing]"), "unresolved callees always rendered (N1)");
    }

    /// Fence language matches the target's actual source language, not a
    /// hardcoded "python" left over from when maple only parsed Python.
    #[test]
    fn render_prompt_fences_use_the_target_files_language() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.rs"), "pub fn target() -> i32 {\n    1\n}\n").unwrap();
        let mut s = store::Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        let bundle = s.bundle("target", 16000, 20, 1, 3, &store::ApproxTokenizer).unwrap();
        let out = render_prompt(&bundle);

        assert!(out.contains("```rust"), "rust source fenced as rust, not python: {out}");
        assert!(!out.contains("```python"), "no stray python fence for a rust file: {out}");
    }

    /// W2.4 — `--snippet-radius` widens/narrows the non-test snippet; `meta.tokenizer` reports the
    /// active tokenizer's name.
    #[test]
    fn w24_snippet_radius_and_tokenizer_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.py"), "def target():\n    return 1\n").unwrap();
        fs::write(root.join("app.py"), "def far_above():\n    pass\n\n\ndef caller():\n    target()\n").unwrap();
        let mut s = store::Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        let narrow = s.bundle("target", 16000, 20, 1, 1, &store::ApproxTokenizer).unwrap();
        let wide = s.bundle("target", 16000, 20, 1, 5, &store::ApproxTokenizer).unwrap();
        assert!(!narrow.callers[0].call_site.snippet.contains("far_above"), "radius=1 excludes the distant def");
        assert!(wide.callers[0].call_site.snippet.contains("far_above"), "radius=5 reaches it");
        assert_eq!(narrow.meta.tokenizer, "approx");
    }

    /// T10.4/L1.1 — `maple parse` on an unregistered extension is a clear error listing the
    /// supported set; every registered extension resolves to its language.
    /// L2.1: `.sql` without a dialect names the flag; with one it resolves to that dialect's lang;
    /// Oracle-only extensions never need the flag.
    #[test]
    fn t10_unknown_language_errors() {
        let err = require_supported("foo.txt", None).unwrap_err();
        assert!(err.to_string().contains("unsupported language"), "{err}");
        assert!(err.to_string().contains(".py") && err.to_string().contains(".rs"), "{err}");
        assert_eq!(require_supported("foo.py", None).unwrap().name, "python");
        assert_eq!(require_supported("foo.rs", None).unwrap().name, "rust");
        assert_eq!(require_supported("foo.tsx", None).unwrap().name, "typescript");
        assert_eq!(require_supported("foo.h", None).unwrap().name, "c");

        let err = require_supported("foo.sql", None).unwrap_err();
        assert!(err.to_string().contains("--sql-dialect"), "{err}");
        assert_eq!(
            require_supported("foo.sql", Some(parser::SqlDialect::Postgres)).unwrap().name,
            "sql-postgres"
        );
        assert_eq!(require_supported("foo.sql", Some(parser::SqlDialect::Tsql)).unwrap().name, "sql-tsql");
        assert_eq!(require_supported("pkg.pkb", None).unwrap().name, "sql-plsql");
    }

    /// T15 — the shared parse-failure line: silent when there's nothing to report, and always uses
    /// the true `count` (not the possibly-capped file list length — N1).
    #[test]
    fn t15_unparsed_status_line() {
        assert_eq!(unparsed_status_line(0, &[]), None);
        let files = vec!["a.py".to_string(), "b.py".to_string()];
        assert_eq!(
            unparsed_status_line(files.len(), &files),
            Some("2 files unparsed (graph incomplete): a.py, b.py".to_string())
        );
        // capped case: count (12) survives even though the displayed list only has 2 entries
        assert_eq!(
            unparsed_status_line(12, &files),
            Some("12 files unparsed (graph incomplete): a.py, b.py".to_string())
        );
    }

    /// T14 — `maple gc` refuses to delete without `--yes`, removes `<repo>/.maple` when confirmed,
    /// and is idempotent (a second `gc` on an already-clean repo isn't an error).
    #[test]
    fn t14_gc_requires_yes_and_removes_the_maple_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".maple")).unwrap();
        fs::write(root.join(".maple/graph.db"), b"x").unwrap();

        let err = gc_maple_dir(root, false).unwrap_err();
        assert!(err.to_string().contains("--yes"), "{err}");
        assert!(root.join(".maple").exists(), "no --yes -> nothing removed");

        let msg = gc_maple_dir(root, true).unwrap();
        assert!(msg.contains("removed"), "{msg}");
        assert!(!root.join(".maple").exists(), "--yes removes it");

        let msg2 = gc_maple_dir(root, true).unwrap();
        assert!(msg2.contains("nothing to remove"), "{msg2}");
    }

    /// T15 — `--format prompt` warns when the graph has unparsed files; stays silent when it doesn't.
    #[test]
    fn t15_render_prompt_warns_on_unparsed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.py"), "def target():\n    return 1\n").unwrap();
        fs::write(root.join("broken.py"), "@!#$ not python {{{ ]]\n").unwrap();
        let mut s = store::Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        let bundle = s.bundle("target", 16000, 20, 1, 3, &store::ApproxTokenizer).unwrap();
        let out = render_prompt(&bundle);
        assert!(out.contains("1 files unparsed (graph incomplete): broken.py"), "{out}");

        // fix it: no more warning
        fs::write(root.join("broken.py"), "def fixed():\n    return 1\n").unwrap();
        s.refresh().unwrap();
        let clean = s.bundle("target", 16000, 20, 1, 3, &store::ApproxTokenizer).unwrap();
        assert!(!render_prompt(&clean).contains("unparsed"), "no warning once the graph has no holes");
    }
}
