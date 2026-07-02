//! S1.2/S1.3 — persistent SQLite store, cold `index`, and edge resolution.
//!
//! S1.2: files/symbols/imports + name index (D1). S1.3: resolve every call-site into exactly one
//! labeled edge (`exact` | `ambiguous` | `unresolved`) with import-alias expansion — upholding the
//! **D0 invariant** (resolution never drops a call-site, only labels it → N1/SC-3). Two-phase index:
//! phase 1 stores all symbols (builds the name index); phase 2 resolves calls against it.

use anyhow::Result;
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

const SKIP_DIRS: &[&str] = &[
    ".git", ".maple", "node_modules", "venv", ".venv", "__pycache__", "dist", "build", "target",
    ".mypy_cache", "vendor",
];

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS files (
  path TEXT PRIMARY KEY, hash TEXT NOT NULL, lang TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS symbols (
  id INTEGER PRIMARY KEY, file TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL,
  parent_class TEXT, start_line INTEGER, end_line INTEGER, signature TEXT,
  ret_class TEXT, base_class TEXT, docstring TEXT, lang TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE TABLE IF NOT EXISTS imports (
  file TEXT NOT NULL, raw TEXT NOT NULL, line INTEGER);
CREATE TABLE IF NOT EXISTS import_names (
  file TEXT NOT NULL, local TEXT NOT NULL, source_module TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_import_names_file ON import_names(file);
CREATE TABLE IF NOT EXISTS edges (
  caller_symbol INTEGER, callee_symbol INTEGER, callee_name TEXT, kind TEXT,
  call_kind TEXT, receiver_class TEXT, call_site_file TEXT, call_site_line INTEGER);
CREATE INDEX IF NOT EXISTS idx_edges_kind ON edges(kind);
CREATE INDEX IF NOT EXISTS idx_edges_callee_name ON edges(callee_name);
CREATE INDEX IF NOT EXISTS idx_edges_caller ON edges(caller_symbol);
CREATE INDEX IF NOT EXISTS idx_edges_receiver ON edges(receiver_class);
CREATE TABLE IF NOT EXISTS parse_failures (
  file TEXT PRIMARY KEY, error TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// T11 — bumped whenever the schema shape changes. `Store::open` compares this against the on-disk
/// db's `PRAGMA user_version`; any mismatch (including a pre-T11 db, which has never set it — 0)
/// drops every table and recreates them empty, rather than risk a raw SQL error from a stale shape
/// (e.g. a query against a column/table that doesn't exist yet). The next `index`/`refresh` rebuilds
/// it from disk — `refresh()` on an empty-files store behaves like a cold walk (see its own doc).
const SCHEMA_VERSION: i32 = 3; // v3: L1.2 — symbols.lang (language-scoped resolution)

const DROP_ALL: &str = "\
DROP TABLE IF EXISTS files;
DROP TABLE IF EXISTS symbols;
DROP TABLE IF EXISTS imports;
DROP TABLE IF EXISTS import_names;
DROP TABLE IF EXISTS edges;
DROP TABLE IF EXISTS parse_failures;
DROP TABLE IF EXISTS meta;
";

#[derive(Debug, Default)]
pub struct IndexStats {
    pub files: usize,
    pub symbols: usize,
    pub imports: usize,
    pub edges: usize,
    pub exact: usize,
    pub ambiguous: usize,
    pub unresolved: usize,
}

#[derive(Debug, Default)]
pub struct RefreshStats {
    pub changed: usize,   // files re-parsed (new or modified)
    pub deleted: usize,   // files removed from the store
    pub relabeled: usize, // pass-B: edges re-resolved because their callee name's def set changed
}

struct CallRec {
    file: String,
    lang: &'static str, // L1.2: the caller file's language — resolution is lang-scoped
    enclosing: String,
    name: String,
    line: i64,
    call_kind: String,
    receiver_class: Option<String>,
}

/// F8 — named alias for `refresh()`'s pass-B relabel query row (clippy::type_complexity).
/// L1.2: the trailing String is the call-site file's `files.lang` (joined in), so pass-B
/// re-resolution stays scoped to the caller's language.
type EdgeRelabelRow = (i64, String, String, Option<i64>, String, Option<String>, String, String);

/// T13 — result of parsing one file's content off the SQLite connection (the part `rayon` runs in
/// parallel: file read + tree-sitter, both pure/CPU, no `Connection` access). The DB-write loop
/// matches on this sequentially and single-threaded (`rusqlite::Connection` is not `Sync`).
enum ParseOutcome {
    /// T15: file couldn't even be read (e.g. permission-denied) — no hash to record.
    Unreadable(String),
    /// the language's walk returned `Err` — hash is still recorded so delta doesn't retry every call.
    ParseErr { hash: String, err: String },
    Ok { hash: String, parsed: crate::parser::ParsedFile, suspect: bool },
}

struct FileParse {
    rel: String,
    lang: &'static str,
    outcome: ParseOutcome,
}

/// T13 — read + parse `path` (relative name `rel`); pure CPU work, safe to run on a rayon thread.
/// L1.1: the walk is dispatched by extension via the registry — `path` is only ever a file the
/// registered-extension walker yielded.
fn parse_one_file(path: &Path, rel: String) -> FileParse {
    let lang = crate::parser::lang_for_path(path).expect("walker only yields registered extensions");
    let outcome = match std::fs::read(path) {
        Err(e) => ParseOutcome::Unreadable(format!("unreadable: {e}")),
        Ok(bytes) => {
            let src = String::from_utf8_lossy(&bytes);
            let hash = hash_bytes(&bytes);
            match (lang.parse)(&src) {
                Err(e) => ParseOutcome::ParseErr { hash, err: e.to_string() },
                Ok(parsed) => {
                    // T15: tree-sitter is error-tolerant (rarely returns Err above) — a non-empty
                    // file that parses to zero defs+calls+imports is the actually-triggerable
                    // signal that something's wrong (garbage/binary content, or a source shape the
                    // shallow walk doesn't extract anything from).
                    let suspect = parsed.defs.is_empty()
                        && parsed.calls.is_empty()
                        && parsed.imports.is_empty()
                        && !src.trim().is_empty();
                    ParseOutcome::Ok { hash, parsed, suspect }
                }
            }
        }
    };
    FileParse { rel, lang: lang.name, outcome }
}

// ---- read-side query results (S1.4) ----------------------------------------

#[derive(Serialize, Debug)]
pub struct SymRef {
    /// T10.1: `<module path>.<name>` — computed at query time, no schema change.
    pub fq_name: String,
    pub name: String,
    pub file: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub docstring: Option<String>,
}

/// L1.1 — does `p` end in a registered source extension? (The generalized `.py` check.)
fn has_src_ext(p: &str) -> bool {
    p.rsplit_once('.').is_some_and(|(_, ext)| crate::parser::is_registered_ext(ext))
}

/// T10.1/F4 — FQ name: module path (file minus its registered extension, `/`→`.`, leading `src/`
/// stripped) + `.` + [`<class>.`]`name` — the parent class segment is inserted when the symbol is
/// a method, so `src/pkg/mod.py` + `Widget` + `render` -> `pkg.mod.Widget.render` instead of
/// dropping the class. Computed at query time; display-only (never changes edge resolution).
fn fq_name(file: &str, name: &str, parent_class: Option<&str>) -> String {
    let no_ext = match file.rsplit_once('.') {
        Some((stem, ext)) if crate::parser::is_registered_ext(ext) => stem,
        _ => file,
    };
    let no_src = no_ext.strip_prefix("src/").unwrap_or(no_ext);
    let module = no_src.replace('/', ".");
    match parent_class {
        Some(c) => format!("{module}.{c}.{name}"),
        None => format!("{module}.{name}"),
    }
}

/// T16 — `--module` accepts a file path in any registered language (`pkg/mod.py`, `src/store.rs`)
/// or a Python dotted path (`pkg.mod`, dots -> `/` + `.py` — dotted module addressing stays a
/// Python convention; other languages use file paths).
fn module_to_file(module: &str) -> String {
    if has_src_ext(module) {
        module.to_string()
    } else {
        format!("{}.py", module.replace('.', "/"))
    }
}

/// T16 — escape SQLite GLOB's special characters (`*`, `?`, `[`) so a `--prefix` match stays a
/// literal, case-sensitive prefix match (N2: never fuzzy). GLOB (unlike LIKE) is case-sensitive,
/// which matters for exact-mode too: Python identifiers are case-sensitive.
fn glob_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[') {
            out.push('[');
            out.push(c);
            out.push(']');
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Serialize, Debug)]
pub struct CallerRef {
    pub caller: String, // enclosing fn name, or "<module>"
    pub file: String,
    pub line: i64,
    pub resolution: String,
}

#[derive(Serialize, Debug)]
pub struct CalleeRef {
    pub name: String,
    pub resolution: String,
    pub file: Option<String>,
    pub start_line: Option<i64>,
    pub signature: Option<String>,
    pub docstring: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct Closure {
    pub symbol: String,
    pub depth: i64,
    pub targets: Vec<SymRef>,
    pub callers: Vec<CallerRef>,
    pub callees: Vec<CalleeRef>,
}

#[derive(Serialize, Debug)]
pub struct Enumeration {
    pub symbol: String,
    pub def_count: i64,
    pub defs: Vec<SymRef>,
    pub caller_count: i64,
    pub caller_file_count: i64,
    pub exact: i64,
    pub ambiguous: i64,
    pub unresolved: i64,
    /// T15: repo-wide count of files that failed to parse cleanly — independent of `symbol`; a
    /// non-zero value means the graph may be missing defs/edges the caller can't otherwise see.
    pub unparsed_files_count: i64,
}

// ---- exists / surface (T16, duplicate-prevention + module-surface queries) ------------------

#[derive(Serialize, Debug)]
pub struct Span {
    pub start_line: i64,
    pub end_line: i64,
}

#[derive(Serialize, Debug)]
pub struct ExistsMatch {
    pub fq_name: String,
    pub kind: String,
    pub file: String,
    pub span: Span,
    pub signature: String,
    pub docstring: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct ImportNameMatch {
    pub file: String,
    pub source_module: String,
}

#[derive(Serialize, Debug)]
pub struct Exists {
    pub name: String,
    pub defs: Vec<ExistsMatch>,
    pub import_names_matches: Vec<ImportNameMatch>,
}

#[derive(Serialize, Debug)]
pub struct SurfaceMethod {
    pub name: String,
    pub kind: String,
    pub span: Span,
    pub signature: String,
    pub docstring: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct SurfaceDef {
    pub name: String,
    pub kind: String, // "function" | "class"
    pub span: Span,
    pub signature: String,
    pub docstring: Option<String>,
    pub methods: Vec<SurfaceMethod>, // empty for functions; a class's methods, nested
}

#[derive(Serialize, Debug)]
pub struct Surface {
    pub module: String,
    pub defs: Vec<SurfaceDef>,
    pub imports: Vec<String>, // raw import lines
    pub unparsed: bool,
}

// ---- impact / blast radius (T9) --------------------------------------------

const IMPACT_CALLER_CAP: usize = 20;

#[derive(Serialize, Debug)]
pub struct ImpactCaller {
    pub caller: String, // enclosing fn name, or "<module>"
    pub file: String,
    pub line: i64,
    pub resolution: String,
}

#[derive(Serialize, Debug)]
pub struct ImpactSymbol {
    pub fq_name: String,
    pub file: String,
    pub start_line: i64,
    pub end_line: i64,
    pub kind: String,
    pub status: String, // "changed" | "deleted" (T9: file removed by the diff)
    pub caller_count: i64,
    pub caller_files: i64,
    pub callers: Vec<ImpactCaller>, // capped IMPACT_CALLER_CAP
    pub callers_omitted: i64,
}

#[derive(Serialize, Debug)]
pub struct Impact {
    pub changed_symbols: Vec<ImpactSymbol>,
    pub files_no_symbols: Vec<String>,
}

// ---- bundle (S1.5, D4 schema) ----------------------------------------------

#[derive(Serialize, Debug)]
pub struct BundleTarget {
    pub fq_name: String,
    pub file: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub docstring: Option<String>,
    pub body: String,
}

#[derive(Serialize, Debug)]
pub struct BundleCallee {
    pub name: String,
    pub resolution: String,
    pub file: Option<String>,
    pub signature: Option<String>,
    pub docstring: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct BundleCallSite {
    pub file: String,
    pub line: i64,
    pub snippet: String,
}

#[derive(Serialize, Debug)]
pub struct BundleCaller {
    pub caller: String,
    pub resolution: String,
    pub is_test: bool, // W2.2: tests-first ordering signal, computed at bundle time
    pub call_site: BundleCallSite,
}

#[derive(Serialize, Debug)]
pub struct BundleReport {
    pub token_count: usize,
    pub budget: i64,
    pub over_budget: bool,
    pub caller_count: usize,      // FULL fan-in (SC-3b) — before the cap
    pub callers_included: usize,  // how many snippets are in this bundle
    pub test_caller_count: usize, // W2.2: full fan-in of test callers (never evicted by the cap)
    pub omitted: Vec<String>,     // capped callers (file:line) — reported, never silent (N1)
    pub ambiguous: Vec<String>,
    pub unresolved: Vec<String>,
    /// T15: repo-wide unparsed files (holes in the graph), capped at 10 — `unparsed_files_count`
    /// is the true total so the cap is never mistaken for the full picture (N1).
    pub unparsed_files: Vec<String>,
    pub unparsed_files_count: usize,
}

#[derive(Serialize, Debug)]
pub struct BundleMeta {
    pub depth: i64,
    pub tokenizer: String,
    pub ambiguous_target: bool,
}

#[derive(Serialize, Debug)]
pub struct Bundle {
    pub target: BundleTarget,
    pub callees: Vec<BundleCallee>,
    pub callers: Vec<BundleCaller>,
    pub report: BundleReport,
    pub meta: BundleMeta,
}

pub struct Store {
    pub conn: Connection,
    root: PathBuf,
}

impl Store {
    pub fn open(repo_root: &Path) -> Result<Self> {
        // F2: fail clearly before create_dir_all ever runs, instead of silently creating `.maple`
        // under whatever `repo_root` happens to resolve to (a typo'd path, a relative-path surprise).
        if !repo_root.exists() {
            anyhow::bail!("repo path does not exist: {}", repo_root.display());
        }
        if !repo_root.is_dir() {
            anyhow::bail!("repo path is not a directory: {}", repo_root.display());
        }
        let dir = repo_root.join(".maple");
        if dir.exists() && !dir.is_dir() {
            anyhow::bail!(".maple exists but is not a directory: {}", dir.display());
        }
        std::fs::create_dir_all(&dir)?;
        let conn = Connection::open(dir.join("graph.db"))?;
        // F1: concurrent `index`/`refresh` writers wait (up to 10s) for SQLite's write lock instead
        // of failing immediately with SQLITE_BUSY — pairs with index_repo's single-transaction
        // commit, which is what actually makes a second writer's wait resolve into one correct
        // graph rather than a lock error. Per-connection pragma, so every open() must set it.
        conn.pragma_update(None, "busy_timeout", 10_000_i32)?;
        // T11: schema versioning — a mismatch (stale db from a different maple build, including a
        // pre-T11 db that never set user_version at all — 0) resets to a known-good empty schema
        // instead of ever surfacing a raw SQL error from a shape mismatch later. Only warn when
        // there was actually pre-existing data to reset (a brand-new file is silently versioned —
        // nothing was lost, nothing to warn about).
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            let had_data: bool = conn
                .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='files' LIMIT 1", [], |_| Ok(()))
                .optional()?
                .is_some();
            if had_data {
                eprintln!(
                    "maple: schema changed (v{version} -> v{SCHEMA_VERSION}); resetting store — reindex required"
                );
            }
            conn.execute_batch(DROP_ALL)?;
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self { conn, root: repo_root.to_path_buf() })
    }

    /// Cold full index: parse all registered-language files, persist symbols/imports, then resolve edges.
    ///
    /// F1 — CRITICAL: clearing the old graph and writing the new one happen inside ONE transaction,
    /// entered only after parsing (pure CPU, no db access) is already done. Two correctness
    /// consequences that follow directly from that: (1) a second `index_repo` racing this one on
    /// the same store serializes at SQLite's write lock (the `busy_timeout` set in `open()` makes it
    /// wait instead of erroring) and whichever transaction commits last is the final graph — never a
    /// doubled one, since each commit both clears and fully repopulates; (2) a process killed (e.g.
    /// SIGINT) before commit leaves the on-disk db exactly as it was before this call — SQLite rolls
    /// back an uncommitted transaction on next open — instead of the old un-transacted `clear()`,
    /// which could wipe the graph and then be interrupted before the rebuild finished.
    pub fn index_repo(&mut self, root: &Path) -> Result<IndexStats> {
        let files = source_files(root);
        let mut st = IndexStats::default();

        // T13: parse (file read + tree-sitter, pure CPU) in parallel across files; every SQLite
        // write below stays single-threaded on the one open `Connection` (not `Sync`) and in the
        // same file order as before, so the persisted graph is unaffected by parallelism.
        let parsed_files: Vec<FileParse> = files
            .par_iter()
            .map(|path| {
                let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_string();
                parse_one_file(path, rel)
            })
            .collect();

        let mut caller_ids: HashMap<(String, String), i64> = HashMap::new();
        let mut alias_maps: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut calls: Vec<CallRec> = Vec::new();

        let tx = self.conn.transaction()?;
        {
            // F1: the clear lives INSIDE this transaction, after parsing — see the doc comment above.
            tx.execute_batch(
                "DELETE FROM files; DELETE FROM symbols; DELETE FROM imports; \
                 DELETE FROM import_names; DELETE FROM edges; DELETE FROM parse_failures;",
            )?;
            let mut fstmt = tx.prepare("INSERT OR REPLACE INTO files(path,hash,lang) VALUES(?1,?2,?3)")?;
            let mut sstmt = tx.prepare(
                "INSERT INTO symbols(file,name,kind,parent_class,start_line,end_line,signature,ret_class,base_class,docstring,lang) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            )?;
            let mut istmt = tx.prepare("INSERT INTO imports(file,raw,line) VALUES(?1,?2,?3)")?;
            let mut nstmt =
                tx.prepare("INSERT INTO import_names(file,local,source_module) VALUES(?1,?2,?3)")?;
            let mut pfstmt = tx.prepare("INSERT INTO parse_failures(file,error) VALUES(?1,?2)")?;

            // Phase 1: symbols/imports + alias maps, collect call-sites (DB writes only — the
            // parse work already ran above, in parallel).
            for fp in &parsed_files {
                let rel = &fp.rel;
                match &fp.outcome {
                    ParseOutcome::Unreadable(err) => {
                        // T15: genuinely unreadable (e.g. permission-denied) — no hash to record
                        // (nothing was read), so this path is re-attempted every refresh call.
                        pfstmt.execute(params![rel, err])?;
                        continue;
                    }
                    ParseOutcome::ParseErr { hash, err } => {
                        eprintln!("! parse {}: {err}", root.join(rel).display());
                        // T15: record the hash anyway so delta doesn't retry the parse every call
                        // — only a content change re-triggers it.
                        fstmt.execute(params![rel, hash, fp.lang])?;
                        pfstmt.execute(params![rel, format!("parse error: {err}")])?;
                        continue;
                    }
                    ParseOutcome::Ok { hash, parsed, suspect } => {
                        fstmt.execute(params![rel, hash, fp.lang])?;
                        if *suspect {
                            pfstmt.execute(params![
                                rel,
                                "suspect: parsed with zero defs/calls/imports for a non-empty file"
                            ])?;
                        }
                        for d in &parsed.defs {
                            sstmt.execute(params![rel, d.name, d.kind, d.parent_class, d.start_line as i64, d.end_line as i64, d.signature, d.ret_class, d.base_class, d.docstring, fp.lang])?;
                            let id = tx.last_insert_rowid();
                            caller_ids.entry((rel.clone(), d.name.clone())).or_insert(id);
                            st.symbols += 1;
                        }
                        for im in &parsed.imports {
                            istmt.execute(params![rel, im.raw, im.line as i64])?;
                            st.imports += 1;
                        }
                        for n in &parsed.import_names {
                            nstmt.execute(params![rel, n.local, n.source_module])?;
                        }
                        if !parsed.aliases.is_empty() {
                            let m = alias_maps.entry(rel.clone()).or_default();
                            for a in &parsed.aliases {
                                m.insert(a.local.clone(), a.source.clone());
                            }
                        }
                        for c in &parsed.calls {
                            calls.push(CallRec {
                                file: rel.clone(),
                                lang: fp.lang,
                                enclosing: c.enclosing.clone(),
                                name: c.name.clone(),
                                line: c.line as i64,
                                call_kind: c.kind.clone(),
                                receiver_class: c.receiver_class.clone(),
                            });
                        }
                    }
                }
                st.files += 1;
            }

            // Phase 2: resolve every call-site into exactly one edge (D0 — never dropped, only labeled).
            // Uses the same resolve_call as the refresh path — cold index and delta stay equivalent
            // by construction.
            let mut estmt = tx.prepare(
                "INSERT INTO edges(caller_symbol,callee_symbol,callee_name,kind,call_kind,receiver_class,call_site_file,call_site_line) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for c in &calls {
                // alias expansion: `baz` -> `bar` if imported `as baz` (avoids under-inclusion silent gap)
                let name = alias_maps
                    .get(&c.file)
                    .and_then(|m| m.get(&c.name))
                    .cloned()
                    .unwrap_or_else(|| c.name.clone());
                let (callee_symbol, label) =
                    resolve_call(&tx, &name, &c.call_kind, c.receiver_class.as_deref(), &c.file, c.lang)?;
                match label {
                    "exact" => st.exact += 1,
                    "ambiguous" => st.ambiguous += 1,
                    _ => st.unresolved += 1,
                }
                let caller = caller_ids.get(&(c.file.clone(), c.enclosing.clone())).copied();
                estmt.execute(params![caller, callee_symbol, name, label, c.call_kind, c.receiver_class, c.file, c.line])?;
                st.edges += 1;
            }

            // T12: record the HEAD this cold index reflects, so the very next refresh() can use
            // the git-aware O(changes) fast path immediately (skipped for non-git repos, or when
            // HEAD can't be read — e.g. a repo with zero commits yet).
            if let Some(head) = git_rev_parse_head(root) {
                tx.execute(
                    "INSERT INTO meta(key,value) VALUES('last_indexed_head',?1) \
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [head],
                )?;
            }
        }
        tx.commit()?;
        debug_assert_eq!(st.edges, st.exact + st.ambiguous + st.unresolved); // D0: one label per edge
        Ok(st)
    }

    pub fn counts(&self) -> Result<(i64, i64, i64)> {
        let f = self.conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let s = self.conn.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
        let i = self.conn.query_row("SELECT COUNT(*) FROM imports", [], |r| r.get(0))?;
        Ok((f, s, i))
    }

    /// (total, exact, ambiguous, unresolved)
    pub fn edge_stats(&self) -> Result<(i64, i64, i64, i64)> {
        let q = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        Ok((
            q("SELECT COUNT(*) FROM edges")?,
            q("SELECT COUNT(*) FROM edges WHERE kind='exact'")?,
            q("SELECT COUNT(*) FROM edges WHERE kind='ambiguous'")?,
            q("SELECT COUNT(*) FROM edges WHERE kind='unresolved'")?,
        ))
    }

    fn defs_named(&self, name: &str) -> Result<Vec<(i64, SymRef)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,name,file,start_line,end_line,signature,docstring,parent_class FROM symbols WHERE name=?1 ORDER BY file,start_line",
        )?;
        let rows = stmt.query_map([name], row_to_symref)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// "X has N callers across M files" + per-resolution counts (SC-3b) — for a planner to partition.
    /// F3: routed through `lookup_targets`, same as `closure`/`bundle`, so the full target-spec
    /// grammar (`path.py::name`, `path.py:LINE`, `module.path.name`/`Class.method`, bare name)
    /// resolves `defs` to the specific matched definition(s); caller counts stay keyed on the
    /// resolved bare name (unchanged semantics for the plain bare-name form).
    pub fn enumerate(&self, spec: &str) -> Result<Enumeration> {
        let (name, defs_with_ids) = self.lookup_targets(spec)?;
        let defs: Vec<SymRef> = defs_with_ids.into_iter().map(|(_, s)| s).collect();
        let count =
            |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [name.as_str()], |r| r.get(0))?) };
        Ok(Enumeration {
            symbol: name.clone(),
            def_count: defs.len() as i64,
            defs,
            caller_count: count("SELECT COUNT(*) FROM edges WHERE callee_name=?1")?,
            caller_file_count: count("SELECT COUNT(DISTINCT call_site_file) FROM edges WHERE callee_name=?1")?,
            exact: count("SELECT COUNT(*) FROM edges WHERE callee_name=?1 AND kind='exact'")?,
            ambiguous: count("SELECT COUNT(*) FROM edges WHERE callee_name=?1 AND kind='ambiguous'")?,
            unresolved: count("SELECT COUNT(*) FROM edges WHERE callee_name=?1 AND kind='unresolved'")?,
            unparsed_files_count: self.parse_failures()?.len() as i64,
        })
    }

    /// T15 — repo-wide list of files that failed to parse cleanly (unreadable, a genuine parse
    /// error, or "suspect": non-empty content that parsed to zero defs+calls+imports). Sorted for
    /// determinism.
    pub fn parse_failures(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT file FROM parse_failures ORDER BY file")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// T16 — everything an agent must check before creating a new symbol named `name`: existing
    /// defs (any file, any kind) + local names some file already imports under this name. Empty
    /// `defs` means it's safe to create. Deterministic only (N2): exact match by default; `prefix`
    /// widens to a case-sensitive prefix match — never fuzzy/similarity.
    pub fn exists(&self, name: &str, prefix: bool) -> Result<Exists> {
        let pattern = if prefix { format!("{}*", glob_escape(name)) } else { String::new() };
        let (dsql, isql): (&str, &str) = if prefix {
            (
                "SELECT name,kind,file,start_line,end_line,signature,docstring,parent_class FROM symbols \
                 WHERE name GLOB ?1 ORDER BY file,start_line",
                "SELECT DISTINCT file,source_module FROM import_names WHERE local GLOB ?1 ORDER BY file",
            )
        } else {
            (
                "SELECT name,kind,file,start_line,end_line,signature,docstring,parent_class FROM symbols \
                 WHERE name=?1 ORDER BY file,start_line",
                "SELECT DISTINCT file,source_module FROM import_names WHERE local=?1 ORDER BY file",
            )
        };
        let bind = if prefix { pattern.as_str() } else { name };

        let mut dstmt = self.conn.prepare(dsql)?;
        let defs: Vec<ExistsMatch> = dstmt
            .query_map([bind], |r| {
                let n: String = r.get(0)?;
                let file: String = r.get(2)?;
                let parent_class: Option<String> = r.get(7)?;
                Ok(ExistsMatch {
                    fq_name: fq_name(&file, &n, parent_class.as_deref()),
                    kind: r.get(1)?,
                    file,
                    span: Span { start_line: r.get(3)?, end_line: r.get(4)? },
                    signature: r.get(5)?,
                    docstring: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;

        let mut istmt = self.conn.prepare(isql)?;
        let import_names_matches: Vec<ImportNameMatch> = istmt
            .query_map([bind], |r| Ok(ImportNameMatch { file: r.get(0)?, source_module: r.get(1)? }))?
            .collect::<std::result::Result<_, _>>()?;

        Ok(Exists { name: name.to_string(), defs, import_names_matches })
    }

    /// T16 — the API surface of a module an agent is about to extend: module-level defs (functions
    /// and classes, with each class's own methods nested under it — not flattened), raw import lines,
    /// and whether the module is currently flagged unparsed (T15). `module` accepts a file path
    /// (`pkg/mod.py`) or a dotted path (`pkg.mod`). A module with no matching file (not yet created,
    /// or never indexed) returns an empty surface — not an error (Day-0 friendly).
    pub fn surface(&self, module: &str) -> Result<Surface> {
        let file_pat = module_to_file(module);
        let canonical: Option<String> = self
            .conn
            .query_row("SELECT path FROM files WHERE path=?1 OR path LIKE '%/' || ?1 LIMIT 1", [&file_pat], |r| r.get(0))
            .optional()?;
        let file = canonical.unwrap_or(file_pat);

        let mut stmt = self.conn.prepare(
            "SELECT name,kind,start_line,end_line,signature,docstring FROM symbols \
             WHERE file=?1 AND parent_class IS NULL ORDER BY start_line",
        )?;
        let mod_rows: Vec<(String, String, i64, i64, String, Option<String>)> = stmt
            .query_map([&file], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<std::result::Result<_, _>>()?;

        let mut mstmt = self.conn.prepare(
            "SELECT name,kind,start_line,end_line,signature,docstring FROM symbols \
             WHERE file=?1 AND parent_class=?2 ORDER BY start_line",
        )?;
        let mut defs = Vec::with_capacity(mod_rows.len());
        for (name, kind, start_line, end_line, signature, docstring) in mod_rows {
            let methods: Vec<SurfaceMethod> = if kind == "class" {
                mstmt
                    .query_map(params![file, name], |r| {
                        Ok(SurfaceMethod {
                            name: r.get(0)?,
                            kind: r.get(1)?,
                            span: Span { start_line: r.get(2)?, end_line: r.get(3)? },
                            signature: r.get(4)?,
                            docstring: r.get(5)?,
                        })
                    })?
                    .collect::<std::result::Result<_, _>>()?
            } else {
                Vec::new()
            };
            defs.push(SurfaceDef { name, kind, span: Span { start_line, end_line }, signature, docstring, methods });
        }

        let imports: Vec<String> = self
            .conn
            .prepare("SELECT raw FROM imports WHERE file=?1 ORDER BY line")?
            .query_map([&file], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;

        let unparsed = self
            .conn
            .query_row("SELECT 1 FROM parse_failures WHERE file=?1 LIMIT 1", [&file], |_| Ok(()))
            .optional()?
            .is_some();

        Ok(Surface { module: module.to_string(), defs, imports, unparsed })
    }

    /// F13 — resolve a target spec to (bare_name, matching defs). Forms:
    ///   `name` | `path.py::name` | `path.py:LINE` (innermost def containing the line) |
    ///   `module.path.name` (dots -> path) with `Class.method` fallback.
    fn lookup_targets(&self, spec: &str) -> Result<(String, Vec<(i64, SymRef)>)> {
        // T10.3: `module_only` excludes methods (parent_class IS NOT NULL) — used by the dotted
        // module.path.name branch so `pkg.y.foo` doesn't also return `K.foo` from the same file.
        let by_file_and_name = |file: &str, name: &str, module_only: bool| -> Result<Vec<(i64, SymRef)>> {
            let sql = if module_only {
                "SELECT id,name,file,start_line,end_line,signature,docstring,parent_class FROM symbols \
                 WHERE name=?1 AND (file=?2 OR file LIKE '%/' || ?2) AND parent_class IS NULL \
                 ORDER BY file,start_line"
            } else {
                "SELECT id,name,file,start_line,end_line,signature,docstring,parent_class FROM symbols \
                 WHERE name=?1 AND (file=?2 OR file LIKE '%/' || ?2) ORDER BY file,start_line"
            };
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map(params![name, file], row_to_symref)?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
        // path.py::name
        if let Some((file, name)) = spec.split_once("::") {
            return Ok((name.to_string(), by_file_and_name(file, name, false)?));
        }
        // path.<ext>:LINE -> innermost def whose span contains the line (any registered language)
        if let Some((file, line)) = spec.rsplit_once(':') {
            if let (true, Ok(l)) = (has_src_ext(file), line.parse::<i64>()) {
                let mut stmt = self.conn.prepare(
                    "SELECT id,name,file,start_line,end_line,signature,docstring,parent_class FROM symbols \
                     WHERE (file=?1 OR file LIKE '%/' || ?1) AND start_line<=?2 AND end_line>=?2 \
                     ORDER BY (end_line-start_line) ASC LIMIT 1",
                )?;
                let rows: Vec<(i64, SymRef)> = stmt
                    .query_map(params![file, l], row_to_symref)?
                    .collect::<std::result::Result<_, _>>()?;
                let name = rows.first().map(|(_, s)| s.name.clone()).unwrap_or_else(|| spec.to_string());
                return Ok((name, rows));
            }
        }
        // module.path.name (dots -> path), then Class.method fallback
        if let Some((prefix, name)) = spec.rsplit_once('.') {
            let module_file = format!("{}.py", prefix.replace('.', "/"));
            let rows = by_file_and_name(&module_file, name, true)?;
            if !rows.is_empty() {
                return Ok((name.to_string(), rows));
            }
            let class = prefix.rsplit('.').next().unwrap_or(prefix);
            let mut stmt = self.conn.prepare(
                "SELECT id,name,file,start_line,end_line,signature,docstring,parent_class FROM symbols \
                 WHERE name=?1 AND parent_class=?2 ORDER BY file,start_line",
            )?;
            let rows: Vec<(i64, SymRef)> = stmt
                .query_map(params![name, class], row_to_symref)?
                .collect::<std::result::Result<_, _>>()?;
            if !rows.is_empty() {
                return Ok((name.to_string(), rows));
            }
        }
        // bare name
        Ok((spec.to_string(), self.defs_named(spec)?))
    }

    /// Depth-1 closure: target def(s) + direct callers + direct callees. `depth` accepted but v1=1.
    /// `spec` accepts all F13 target forms; callers are matched by the resolved bare name
    /// (the SC-3 safe over-set), callees come from the specific matched def(s).
    pub fn closure(&self, spec: &str, depth: i64) -> Result<Closure> {
        let (name, targets_with_ids) = self.lookup_targets(spec)?;
        let name = name.as_str();
        let target_ids: Vec<i64> = targets_with_ids.iter().map(|(id, _)| *id).collect();
        let targets: Vec<SymRef> = targets_with_ids.into_iter().map(|(_, s)| s).collect();

        // callers: every edge whose resolved name is the target (over-includes ambiguous — safe, flagged)
        let mut cstmt = self.conn.prepare(
            "SELECT COALESCE(s.name,'<module>'), e.call_site_file, e.call_site_line, e.kind \
             FROM edges e LEFT JOIN symbols s ON e.caller_symbol=s.id \
             WHERE e.callee_name=?1 ORDER BY e.call_site_file, e.call_site_line",
        )?;
        let callers: Vec<CallerRef> = cstmt
            .query_map([name], |r| {
                Ok(CallerRef { caller: r.get(0)?, file: r.get(1)?, line: r.get(2)?, resolution: r.get(3)? })
            })?
            .collect::<std::result::Result<_, _>>()?;

        // callees: edges out of each target def id, deduped by callee name
        let mut callees: Vec<CalleeRef> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut estmt = self.conn.prepare(
            "SELECT e.callee_name, e.kind, s.file, s.start_line, s.signature, s.docstring \
             FROM edges e LEFT JOIN symbols s ON e.callee_symbol=s.id \
             WHERE e.caller_symbol=?1",
        )?;
        for id in &target_ids {
            let rows = estmt.query_map([id], |r| {
                Ok(CalleeRef {
                    name: r.get(0)?,
                    resolution: r.get(1)?,
                    file: r.get(2)?,
                    start_line: r.get(3)?,
                    signature: r.get(4)?,
                    docstring: r.get(5)?,
                })
            })?;
            for c in rows {
                let c = c?;
                if seen.insert(c.name.clone()) {
                    callees.push(c);
                }
            }
        }
        Ok(Closure { symbol: name.to_string(), depth, targets, callers, callees })
    }

    /// S1.6 — delta refresh (D2/A1): hash-compare disk vs store, re-parse ONLY changed files,
    /// patch the graph, then two-pass relabel edges whose callee's definition set changed —
    /// including edges from UNCHANGED files (the renamed-symbol case). Queries call this first,
    /// which is the "guaranteed-fresh at query time" contract. No daemon.
    pub fn refresh(&mut self) -> Result<RefreshStats> {
        let root = self.root.clone();
        let mut st = RefreshStats::default();

        // T12: git-aware O(changes) delta. When HEAD hasn't moved since the last index/refresh,
        // trust `git status --porcelain -uall` to name every path that *might* differ and only
        // hash-compare those (instead of walking + hashing the whole tree). `candidate_paths` is
        // `None` — meaning "walk everything" — for a non-git repo, any git error, or a HEAD that
        // has moved (a commit between maple runs would make porcelain empty while the store is
        // stale, so HEAD-changed always falls back to the full walk once). Correctness first: when
        // in doubt, walk.
        let mut candidate_paths: Option<Vec<String>> = None;
        let mut observed_head: Option<String> = None;
        if root.join(".git").exists() {
            if let Some(head) = git_rev_parse_head(&root) {
                let stored_head: Option<String> = self
                    .conn
                    .query_row("SELECT value FROM meta WHERE key='last_indexed_head'", [], |r| r.get(0))
                    .optional()?;
                if stored_head.as_deref() == Some(head.as_str()) {
                    if let Some(status) = git_status_porcelain(&root) {
                        candidate_paths = Some(
                            parse_porcelain_paths(&status)
                                .into_iter()
                                .filter(|p| crate::parser::lang_for_path(Path::new(p)).is_some())
                                .collect(),
                        );
                    }
                }
                observed_head = Some(head);
            }
        }

        let stored: HashMap<String, String> = {
            let mut stmt = self.conn.prepare("SELECT path,hash FROM files")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut disk: HashMap<String, (String, Vec<u8>)> = HashMap::new();
        // T15: files that fail to even read (permission-denied etc.) — recorded separately below,
        // after pass A (which would otherwise immediately wipe a fresh insert here for a file that
        // was previously stored and is now also in `deleted`).
        let mut unreadable: Vec<(String, String)> = Vec::new();
        let (changed, deleted): (Vec<String>, Vec<String>) = if let Some(candidates) = &candidate_paths {
            // fast path: only read+hash the paths git flagged as possibly different.
            for rel in candidates {
                match std::fs::read(root.join(rel)) {
                    Ok(bytes) => {
                        disk.insert(rel.clone(), (hash_bytes(&bytes), bytes));
                    }
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        unreadable.push((rel.clone(), format!("unreadable: {e}")))
                    }
                    Err(_) => {} // gone from disk — a candidate for `deleted` below, or a
                                 // rename/deletion of a path never actually indexed (ignored)
                }
            }
            let changed = disk.iter().filter(|(p, (h, _))| stored.get(*p) != Some(h)).map(|(p, _)| p.clone()).collect();
            let deleted =
                candidates.iter().filter(|p| !disk.contains_key(*p) && stored.contains_key(*p)).cloned().collect();
            (changed, deleted)
        } else {
            // fallback: non-git repo, a git error, or HEAD moved — walk + hash everything.
            for path in source_files(&root) {
                let rel = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().to_string();
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        disk.insert(rel, (hash_bytes(&bytes), bytes));
                    }
                    Err(e) => unreadable.push((rel, format!("unreadable: {e}"))),
                }
            }
            let changed = disk.iter().filter(|(p, (h, _))| stored.get(*p) != Some(h)).map(|(p, _)| p.clone()).collect();
            let deleted = stored.keys().filter(|p| !disk.contains_key(*p)).cloned().collect();
            (changed, deleted)
        };

        if changed.is_empty() && deleted.is_empty() && unreadable.is_empty() {
            // still record the observed HEAD (idempotent) so a HEAD-changed fallback run with a
            // genuinely empty diff still unlocks the fast path on the next call.
            if let Some(head) = &observed_head {
                self.conn.execute(
                    "INSERT INTO meta(key,value) VALUES('last_indexed_head',?1) \
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [head],
                )?;
            }
            return Ok(st);
        }
        st.changed = changed.len();
        st.deleted = deleted.len();

        let tx = self.conn.transaction()?;
        let mut affected: std::collections::HashSet<String> = std::collections::HashSet::new();

        // pass A: drop all rows owned by changed+deleted files (old def names -> affected)
        for f in changed.iter().chain(deleted.iter()) {
            let mut q = tx.prepare("SELECT name FROM symbols WHERE file=?1")?;
            for n in q.query_map([f], |r| r.get::<_, String>(0))? {
                affected.insert(n?);
            }
            tx.execute("DELETE FROM symbols WHERE file=?1", [f])?;
            tx.execute("DELETE FROM imports WHERE file=?1", [f])?;
            tx.execute("DELETE FROM import_names WHERE file=?1", [f])?;
            tx.execute("DELETE FROM edges WHERE call_site_file=?1", [f])?;
            tx.execute("DELETE FROM files WHERE path=?1", [f])?;
            tx.execute("DELETE FROM parse_failures WHERE file=?1", [f])?;
        }

        // T15: (re)record still-unreadable files now that pass A's cleanup (which would also
        // touch these paths if they were previously stored -> now `deleted`) has already run.
        {
            let mut pfstmt = tx.prepare("INSERT OR REPLACE INTO parse_failures(file,error) VALUES(?1,?2)")?;
            for (f, err) in &unreadable {
                pfstmt.execute(params![f, err])?;
            }
        }

        // T13: re-parse changed files' content in parallel (already read+hashed above — this is
        // pure CPU: just the language's walk); the DB-write loop below stays single-threaded and
        // in the same `changed` order as before.
        let parsed_changed: Vec<(String, &'static str, ParseOutcome)> = changed
            .par_iter()
            .map(|f| {
                let lang =
                    crate::parser::lang_for_path(Path::new(f)).expect("changed set only holds registered files");
                let (hash, bytes) = &disk[f];
                let src = String::from_utf8_lossy(bytes);
                let outcome = match (lang.parse)(&src) {
                    Err(e) => ParseOutcome::ParseErr { hash: hash.clone(), err: e.to_string() },
                    Ok(parsed) => {
                        let suspect = parsed.defs.is_empty()
                            && parsed.calls.is_empty()
                            && parsed.imports.is_empty()
                            && !src.trim().is_empty();
                        ParseOutcome::Ok { hash: hash.clone(), parsed, suspect }
                    }
                };
                (f.clone(), lang.name, outcome)
            })
            .collect();

        // insert symbols/imports (new def names -> affected); collect calls
        let mut calls: Vec<CallRec> = Vec::new();
        let mut alias_maps: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut caller_ids: HashMap<(String, String), i64> = HashMap::new();
        for (f, lang, outcome) in &parsed_changed {
            let (hash, parsed, suspect) = match outcome {
                ParseOutcome::Unreadable(_) => unreachable!("`changed` files were just read successfully above"),
                ParseOutcome::ParseErr { hash, err } => {
                    eprintln!("! parse {f}: {err}");
                    // T15: still record the hash so delta doesn't retry this parse every call —
                    // pass A already cleared any stale parse_failures row for this path.
                    tx.execute("INSERT OR REPLACE INTO files(path,hash,lang) VALUES(?1,?2,?3)", params![f, hash, lang])?;
                    tx.execute(
                        "INSERT INTO parse_failures(file,error) VALUES(?1,?2)",
                        params![f, format!("parse error: {err}")],
                    )?;
                    continue;
                }
                ParseOutcome::Ok { hash, parsed, suspect } => (hash, parsed, *suspect),
            };
            tx.execute("INSERT OR REPLACE INTO files(path,hash,lang) VALUES(?1,?2,?3)", params![f, hash, lang])?;
            if suspect {
                tx.execute(
                    "INSERT INTO parse_failures(file,error) VALUES(?1,?2)",
                    params![f, "suspect: parsed with zero defs/calls/imports for a non-empty file"],
                )?;
            }
            for d in &parsed.defs {
                tx.execute(
                    "INSERT INTO symbols(file,name,kind,parent_class,start_line,end_line,signature,ret_class,base_class,docstring,lang) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    params![f, d.name, d.kind, d.parent_class, d.start_line as i64, d.end_line as i64, d.signature, d.ret_class, d.base_class, d.docstring, lang],
                )?;
                let id = tx.last_insert_rowid();
                affected.insert(d.name.clone());
                caller_ids.entry((f.clone(), d.name.clone())).or_insert(id);
            }
            for im in &parsed.imports {
                tx.execute("INSERT INTO imports(file,raw,line) VALUES(?1,?2,?3)", params![f, im.raw, im.line as i64])?;
            }
            for n in &parsed.import_names {
                tx.execute(
                    "INSERT INTO import_names(file,local,source_module) VALUES(?1,?2,?3)",
                    params![f, n.local, n.source_module],
                )?;
            }
            let m = alias_maps.entry(f.clone()).or_default();
            for a in &parsed.aliases {
                m.insert(a.local.clone(), a.source.clone());
            }
            for c in &parsed.calls {
                calls.push(CallRec {
                    file: f.clone(),
                    lang,
                    enclosing: c.enclosing.clone(),
                    name: c.name.clone(),
                    line: c.line as i64,
                    call_kind: c.kind.clone(),
                    receiver_class: c.receiver_class.clone(),
                });
            }
        }

        // insert edges for changed files, resolved against the CURRENT symbols table (D0: one edge per call)
        for c in &calls {
            let name = alias_maps
                .get(&c.file)
                .and_then(|m| m.get(&c.name))
                .cloned()
                .unwrap_or_else(|| c.name.clone());
            let (callee, label) =
                resolve_call(&tx, &name, &c.call_kind, c.receiver_class.as_deref(), &c.file, c.lang)?;
            let caller = caller_ids.get(&(c.file.clone(), c.enclosing.clone())).copied();
            tx.execute(
                "INSERT INTO edges(caller_symbol,callee_symbol,callee_name,kind,call_kind,receiver_class,call_site_file,call_site_line) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![caller, callee, name, label, c.call_kind, c.receiver_class, c.file, c.line],
            )?;
        }

        // pass B: relabel edges (incl. from unchanged files) affected by the changed definition set —
        // matched by callee_name OR receiver_class (a class rename shifts its methods' resolution too).
        // Per-edge, because resolution depends on stored call_kind + receiver_class context.
        {
            // L1.2: join `files` for each edge's language — pass-B re-resolution must stay scoped
            // to the caller file's language, same as the original resolution.
            let mut sel = tx.prepare(
                "SELECT e.rowid, e.callee_name, e.kind, e.callee_symbol, e.call_kind, e.receiver_class, \
                        e.call_site_file, f.lang \
                 FROM edges e JOIN files f ON e.call_site_file=f.path \
                 WHERE e.callee_name=?1 OR e.receiver_class=?1",
            )?;
            let mut upd = tx.prepare("UPDATE edges SET kind=?1, callee_symbol=?2 WHERE rowid=?3")?;
            for name in &affected {
                let rows: Vec<EdgeRelabelRow> = sel
                    .query_map([name], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
                    })?
                    .collect::<std::result::Result<_, _>>()?;
                for (rowid, callee_name, old_kind, old_sym, call_kind, receiver_class, call_site_file, lang) in rows {
                    let (callee, label) =
                        resolve_call(&tx, &callee_name, &call_kind, receiver_class.as_deref(), &call_site_file, &lang)?;
                    if label != old_kind || callee != old_sym {
                        upd.execute(params![label, callee, rowid])?;
                        st.relabeled += 1;
                    }
                }
            }
        }

        // T12: record the HEAD this refresh reflects, atomically with the rest of the update, so
        // the next call can trust the git-aware fast path from here.
        if let Some(head) = &observed_head {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES('last_indexed_head',?1) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [head],
            )?;
        }
        tx.commit()?;
        Ok(st)
    }

    /// S1.5 — assemble the D4 context bundle. Caps callers at `caller_cap` (fan-in bound, S0.2)
    /// and reports the FULL fan-in + omitted callers — capped, never silently dropped (N1).
    /// W2.2: callers sort tests-first (stable) so the cap never evicts a test caller in favor of
    /// a non-test one; test callers get the full enclosing-function body (capped) instead of ±radius.
    pub fn bundle(
        &self,
        name: &str,
        budget: i64,
        caller_cap: usize,
        depth: i64,
        snippet_radius: i64,
        tokenizer: &dyn Tokenizer,
    ) -> Result<Bundle> {
        let mut cl = self.closure(name, depth)?;
        let t = cl
            .targets
            .first()
            .ok_or_else(|| anyhow::anyhow!("symbol not found: no definition named {name:?}"))?;
        let ambiguous_target = cl.targets.len() > 1;
        let target = BundleTarget {
            // F4: `t.fq_name` already carries the class segment (computed in `row_to_symref`) —
            // recomputing here would need `t`'s parent_class again, which `SymRef` doesn't expose.
            fq_name: t.fq_name.clone(),
            file: t.file.clone(),
            start_line: t.start_line,
            end_line: t.end_line,
            signature: t.signature.clone(),
            docstring: t.docstring.clone(),
            body: read_span(&self.root.join(&t.file), t.start_line, t.end_line),
        };

        let callees: Vec<BundleCallee> = cl
            .callees
            .iter()
            .map(|c| BundleCallee {
                name: c.name.clone(),
                resolution: c.resolution.clone(),
                file: c.file.clone(),
                signature: c.signature.clone(),
                docstring: c.docstring.clone(),
            })
            .collect();

        // W2.2: tests first, then the existing file/line order (closure() already sorts by
        // file,line; sort_by_key is stable so that secondary order survives).
        cl.callers.sort_by_key(|c| !is_test_file(&c.file));
        let test_caller_count = cl.callers.iter().filter(|c| is_test_file(&c.file)).count();

        let caller_count = cl.callers.len();
        let included = caller_count.min(caller_cap);
        let mut callers: Vec<BundleCaller> = Vec::with_capacity(included);
        for c in &cl.callers[..included] {
            let is_test = is_test_file(&c.file);
            let path = self.root.join(&c.file);
            let snippet = if is_test {
                self.caller_span(&cl.symbol, &c.file, c.line)?
                    .map(|(s, e)| read_span(&path, s, e))
                    .filter(|body| !body.is_empty())
                    .map(|body| body.lines().take(60).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_else(|| read_snippet(&path, c.line, snippet_radius))
            } else {
                read_snippet(&path, c.line, snippet_radius)
            };
            callers.push(BundleCaller {
                caller: c.caller.clone(),
                resolution: c.resolution.clone(),
                is_test,
                call_site: BundleCallSite { file: c.file.clone(), line: c.line, snippet },
            });
        }
        let omitted: Vec<String> =
            cl.callers[included..].iter().map(|c| format!("{}:{}", c.file, c.line)).collect();

        let mut ambiguous: Vec<String> =
            callees.iter().filter(|c| c.resolution == "ambiguous").map(|c| c.name.clone()).collect();
        if ambiguous_target {
            ambiguous.push(format!("<target:{name}>")); // target name itself is ambiguous
        }
        let unresolved: Vec<String> =
            callees.iter().filter(|c| c.resolution == "unresolved").map(|c| c.name.clone()).collect();

        let all_unparsed = self.parse_failures()?;
        let unparsed_files_count = all_unparsed.len();
        let unparsed_files: Vec<String> = all_unparsed.into_iter().take(10).collect();

        let mut b = Bundle {
            target,
            callees,
            callers,
            report: BundleReport {
                token_count: 0,
                budget,
                over_budget: false,
                caller_count,
                callers_included: included,
                test_caller_count,
                omitted,
                ambiguous,
                unresolved,
                unparsed_files,
                unparsed_files_count,
            },
            meta: BundleMeta { depth, tokenizer: tokenizer.name().to_string(), ambiguous_target },
        };
        // token count over the actual JSON payload the agent receives (D5: approximate, a bound)
        let approx = tokenizer.count(&serde_json::to_string(&b)?);
        b.report.token_count = approx;
        b.report.over_budget = (approx as i64) > budget; // signal only, never silent trim
        Ok(b)
    }

    /// W2.2: the enclosing symbol's span for a caller's call-site, via `edges.caller_symbol` —
    /// module-level callers (no enclosing def) return `None`, which falls back to ±radius.
    fn caller_span(&self, callee_name: &str, file: &str, line: i64) -> Result<Option<(i64, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.start_line, s.end_line FROM edges e JOIN symbols s ON e.caller_symbol=s.id \
                 WHERE e.callee_name=?1 AND e.call_site_file=?2 AND e.call_site_line=?3 LIMIT 1",
                params![callee_name, file, line],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// T9 — blast radius of a change: `changed` maps repo-relative file -> changed NEW-version line
    /// ranges (from parsed diff hunk headers); `deleted_files` lists files removed by the diff.
    /// Deleted files' symbol identities are captured BEFORE `refresh()` (which drops their rows) —
    /// this only sees a deletion if the store still holds pre-deletion rows for it (i.e. no earlier
    /// refresh already dropped them); reconstructing from the git blob is out of scope for T9 (no
    /// test requires it). `refresh()` runs next (A1), then changed files are matched against the
    /// now-current symbol spans.
    pub fn impact(&mut self, changed: &HashMap<String, Vec<(i64, i64)>>, deleted_files: &[String]) -> Result<Impact> {
        let mut deleted_syms: Vec<SymRow> = Vec::new();
        for f in deleted_files {
            deleted_syms.extend(read_syms(&self.conn, f)?);
        }

        self.refresh()?;

        let mut changed_symbols = Vec::new();
        let mut files_no_symbols = Vec::new();
        let mut files: Vec<&String> = changed.keys().collect();
        files.sort();
        for file in files {
            let ranges = &changed[file];
            let mut hit = false;
            for sym in read_syms(&self.conn, file)? {
                if ranges.iter().any(|(rs, re)| sym.start <= *re && sym.end >= *rs) {
                    hit = true;
                    changed_symbols.push(self.impact_entry(&sym, "changed")?);
                }
            }
            if !hit {
                files_no_symbols.push(file.clone());
            }
        }
        for sym in &deleted_syms {
            changed_symbols.push(self.impact_entry(sym, "deleted")?);
        }

        Ok(Impact { changed_symbols, files_no_symbols })
    }

    /// T9 — one symbol's blast-radius entry: full caller fan-in, capped 20, full count always
    /// reported (N1 spirit — never silent). Callers matched by resolved name, same safe over-set
    /// pattern as `closure()`.
    fn impact_entry(&self, sym: &SymRow, status: &str) -> Result<ImpactSymbol> {
        let mut cstmt = self.conn.prepare(
            "SELECT COALESCE(s.name,'<module>'), e.call_site_file, e.call_site_line, e.kind \
             FROM edges e LEFT JOIN symbols s ON e.caller_symbol=s.id \
             WHERE e.callee_name=?1 ORDER BY e.call_site_file, e.call_site_line",
        )?;
        let all: Vec<ImpactCaller> = cstmt
            .query_map([&sym.name], |r| {
                Ok(ImpactCaller { caller: r.get(0)?, file: r.get(1)?, line: r.get(2)?, resolution: r.get(3)? })
            })?
            .collect::<std::result::Result<_, _>>()?;
        let caller_count = all.len() as i64;
        let caller_files = all.iter().map(|c| c.file.as_str()).collect::<std::collections::HashSet<_>>().len() as i64;
        let callers_omitted = all.len().saturating_sub(IMPACT_CALLER_CAP) as i64;
        let callers: Vec<ImpactCaller> = all.into_iter().take(IMPACT_CALLER_CAP).collect();
        Ok(ImpactSymbol {
            fq_name: fq_name(&sym.file, &sym.name, sym.parent_class.as_deref()),
            file: sym.file.clone(),
            start_line: sym.start,
            end_line: sym.end,
            kind: sym.kind.clone(),
            status: status.to_string(),
            caller_count,
            caller_files,
            callers,
            callers_omitted,
        })
    }
}

/// F4 — one `symbols` row projected for `impact()`'s blast-radius matching; named fields instead of
/// a growing tuple keep `impact_entry`'s argument count sane (clippy::too_many_arguments).
struct SymRow {
    name: String,
    file: String,
    kind: String,
    start: i64,
    end: i64,
    parent_class: Option<String>,
}

/// T9/F4 — every symbol defined in `file`, for `impact()`'s blast-radius matching. A free function
/// (not a `&self` method) so `impact()` can call it both before and after its own `self.refresh()`
/// without holding a borrow of `self` across that mutable call.
fn read_syms(conn: &Connection, file: &str) -> Result<Vec<SymRow>> {
    let mut stmt = conn.prepare(
        "SELECT name,file,kind,start_line,end_line,parent_class FROM symbols WHERE file=?1 ORDER BY start_line",
    )?;
    let rows = stmt.query_map([file], |r| {
        Ok(SymRow {
            name: r.get(0)?,
            file: r.get(1)?,
            kind: r.get(2)?,
            start: r.get(3)?,
            end: r.get(4)?,
            parent_class: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// T17 — worktree warm-start: copy `<source>/.maple/graph.db` into `<target>/.maple/`, then
/// `refresh()` against the target tree so the copied graph catches up to the target's actual file
/// state — O(branch-diff) instead of a cold `index_repo` (O(repo)). Refuses to clobber an existing
/// target index unless `force`.
pub fn seed(target: &Path, source: &Path, force: bool) -> Result<RefreshStats> {
    let target_dir = target.join(".maple");
    let target_db = target_dir.join("graph.db");
    if target_db.exists() && !force {
        anyhow::bail!("target already has an index at {} — pass --force to overwrite", target_db.display());
    }
    let source_db = source.join(".maple").join("graph.db");
    if !source_db.exists() {
        anyhow::bail!(
            "source has no index at {} — run `maple index {}` first",
            source_db.display(),
            source.display()
        );
    }
    std::fs::create_dir_all(&target_dir)?;
    std::fs::copy(&source_db, &target_db)?;
    let mut s = Store::open(target)?;
    s.refresh()
}

/// T12 — `git -C root rev-parse HEAD`; `None` on any error (bad/missing repo, zero commits yet, git
/// not installed) — callers treat that as "can't trust git," falling back to the full hash walk
/// (correctness first: when in doubt, walk).
fn git_rev_parse_head(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(root).arg("rev-parse").arg("HEAD").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// T12 — `git -C root status --porcelain -uall` (every untracked file listed individually, not
/// summarized directories); `None` on any error (same fallback contract as `git_rev_parse_head`).
fn git_status_porcelain(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("status")
        .arg("--porcelain")
        .arg("-uall")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// T12 — porcelain v1 line shape is `XY<space>PATH`, or `XY<space>OLD -> NEW` for a detected
/// rename; every path mentioned becomes a candidate (the old side of a rename disappearing is how
/// a same-refresh rename shows up as a deletion+addition — same effect as editing two files).
/// ponytail: doesn't unescape git's quoted-path form (rare non-ASCII/quote/backslash filenames —
/// Python module names are conventionally ASCII identifiers, so this is a narrow gap); a missed
/// candidate there just stays stale until the next HEAD move or full reindex, upgrade to unescaping
/// if that ever bites.
fn parse_porcelain_paths(status: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in status.lines() {
        if line.len() < 4 {
            continue;
        }
        let rest = line[3..].trim();
        if let Some((old, new)) = rest.split_once(" -> ") {
            paths.push(old.trim_matches('"').to_string());
            paths.push(new.trim_matches('"').to_string());
        } else {
            paths.push(rest.trim_matches('"').to_string());
        }
    }
    paths
}

/// T9 — `git -C <repo> diff --unified=0 <rev>` (or `--staged`); working tree vs `<rev>`. A bad rev
/// or a non-git `<repo>` surfaces as git's own stderr, wrapped in a clear error (non-zero exit).
pub fn run_git_diff(repo: &str, rev: Option<&str>, staged: bool) -> Result<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo).arg("diff").arg("--unified=0");
    if staged {
        cmd.arg("--staged");
    } else if let Some(r) = rev {
        cmd.arg(r);
    } else {
        anyhow::bail!("maple impact: either --diff <rev> or --staged is required");
    }
    let out = cmd.output().map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git diff failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// F8 — named alias for `parse_diff`'s changed-ranges map (clippy::type_complexity).
type ChangedRanges = HashMap<String, Vec<(i64, i64)>>;

/// T9 — parse `git diff --unified=0` output into (file -> changed NEW-version line ranges, deleted
/// files). Hand-rolled (no regex dep — K2): the hunk header is a fixed `@@ -a[,b] +c[,d] @@` shape.
pub fn parse_diff(diff: &str) -> (ChangedRanges, Vec<String>) {
    let mut changed: ChangedRanges = HashMap::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut old_path: Option<String> = None;
    let mut cur_file: Option<String> = None;
    let mut cur_deleted = false;

    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("--- ") {
            old_path = strip_ab_prefix(p);
        } else if let Some(p) = line.strip_prefix("+++ ") {
            if p.trim() == "/dev/null" {
                cur_deleted = true;
                if let Some(f) = old_path.clone() {
                    deleted.push(f);
                }
            } else {
                cur_deleted = false;
                cur_file = strip_ab_prefix(p);
            }
        } else if line.starts_with("@@ ") && !cur_deleted {
            if let (Some(file), Some((start, len))) = (&cur_file, parse_hunk_header(line)) {
                // len==0 (pure deletion, no new lines added): `start` is the new-file line the
                // deletion sits at — use it as a single-point marker (ponytail: exact boundary
                // semantics for the zero-context case are an edge case no required test covers).
                let range = if len == 0 { (start, start) } else { (start, start + len - 1) };
                changed.entry(file.clone()).or_default().push(range);
            }
        }
    }
    (changed, deleted)
}

/// `@@ -a[,b] +c[,d] @@ …` -> (c, d). `,d` defaults to 1 when omitted (single-line hunk).
fn parse_hunk_header(line: &str) -> Option<(i64, i64)> {
    let rest = line.strip_prefix("@@ -")?;
    let plus_at = rest.find(" +")?;
    let after_plus = &rest[plus_at + 2..];
    let end_at = after_plus.find(" @@")?;
    let new_part = &after_plus[..end_at];
    let (start_str, len_str) = new_part.split_once(',').unwrap_or((new_part, "1"));
    Some((start_str.parse().ok()?, len_str.parse().ok()?))
}

/// Strip git's `a/`/`b/` diff prefix; `/dev/null` (new/deleted file marker) -> None.
fn strip_ab_prefix(p: &str) -> Option<String> {
    let p = p.trim();
    if p == "/dev/null" {
        return None;
    }
    Some(p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string())
}

/// Resolve a (post-alias) call against the symbols table: (callee_symbol, label).
/// The label domain is exactly {exact, ambiguous, unresolved} — never a score (N2).
///
/// L1.2: resolution is LANGUAGE-SCOPED — every candidate query filters to the CALLER's language,
/// so a `.rs` call can never match a `.java` def. Cross-language calls (FFI, subprocess) are
/// honest `unresolved`.
///
/// Universal tier: candidate set = all same-language defs with this name. S2/T1-T4 Python resolver
/// (in-process seam), narrowing ONLY — never widens or drops (D0), falls back to the universal
/// answer when a rule doesn't apply:
///  - method call with a receiver-class hint (self.foo / x = C(); x.foo / C().foo / T1 type
///    annotations / T2 self.attr): filter to methods of that class — applied only when the hint
///    uniquely names one class symbol. If the class doesn't define it itself, T4 tries exactly one
///    inheritance hop via its base_class.
///  - bare `foo()`: Python scoping cannot bind a bare name to a method implicitly → filter to
///    module-level (non-method) defs, preferring (T3) the caller file's imports, then its own
///    module-level def, before falling back to the blind global filter.
fn resolve_call(
    conn: &Connection,
    name: &str,
    call_kind: &str,
    receiver_class: Option<&str>,
    call_site_file: &str,
    lang: &str,
) -> Result<(Option<i64>, &'static str)> {
    let mut stmt =
        conn.prepare_cached("SELECT id, parent_class, file FROM symbols WHERE name=?1 AND lang=?2")?;
    let cands: Vec<(i64, Option<String>, String)> = stmt
        .query_map(params![name, lang], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if cands.is_empty() {
        return Ok((None, "unresolved"));
    }

    if call_kind == "method" {
        if let Some(rc) = receiver_class {
            // hint is trusted only if `rc` names exactly one same-language symbol and it's a class
            let mut cstmt = conn.prepare_cached("SELECT kind FROM symbols WHERE name=?1 AND lang=?2 LIMIT 2")?;
            let kinds: Vec<String> = cstmt
                .query_map(params![rc, lang], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            if kinds.is_empty() {
                // W2.1: `rc` is provably external (builtin, or an import whose source module has
                // no in-repo file) -> unresolved, not ambiguous noise. Never guess: requires BOTH
                // zero in-repo symbols named `rc` (checked above) AND a builtin/import proof —
                // validation ambiguity alone (e.g. an unbound ctor-name guess) is not proof.
                // Only displaces the would-be-AMBIGUOUS fallback (cands.len() > 1): exact must not
                // decrease (D0 constitution), so a lone same-named in-repo candidate keeps its
                // existing exact match even when the receiver is external.
                // L1.2: Python-only — the builtin list and import-module matching are Python facts.
                if cands.len() > 1 && lang == "python" && is_external_receiver(conn, rc, call_site_file)? {
                    return Ok((None, "unresolved"));
                }
            } else if kinds.len() == 1 && kinds[0] == "class" {
                let filt: Vec<&(i64, Option<String>, String)> =
                    cands.iter().filter(|(_, pc, _)| pc.as_deref() == Some(rc)).collect();
                match filt.len() {
                    1 => return Ok((Some(filt[0].0), "exact")),
                    n if n > 1 => return Ok((None, "ambiguous")),
                    _ => {
                        // T4: rc doesn't define it itself — try exactly one inheritance hop
                        if let Some(hit) = base_class_hop(conn, rc, &cands, lang)? {
                            return Ok(hit);
                        }
                    } // no base, ambiguous base, or base lacks it -> universal fallback
                }
            }
        }
    } else if call_kind == "func" {
        let mod_level: Vec<&(i64, Option<String>, String)> =
            cands.iter().filter(|(_, pc, _)| pc.is_none()).collect();

        // T3(a): caller file imports `name` from module M -> prefer defs in files matching M,
        // plus any same-file (call_site_file) def of `name`.
        let import_module: Option<String> = conn
            .prepare_cached("SELECT source_module FROM import_names WHERE file=?1 AND local=?2 LIMIT 1")?
            .query_row(params![call_site_file, name], |r| r.get(0))
            .optional()?;
        if let Some(m) = import_module {
            let suffix = format!("{}.py", m.replace('.', "/"));
            let filt: Vec<&&(i64, Option<String>, String)> = mod_level
                .iter()
                .filter(|(_, _, f)| f == call_site_file || *f == suffix || f.ends_with(&format!("/{suffix}")))
                .collect();
            match filt.len() {
                1 => return Ok((Some(filt[0].0), "exact")),
                n if n > 1 => return Ok((None, "ambiguous")),
                _ => {} // imported module not found in repo -> fall through to (b)/(c)
            }
        }
        // T3(b): Python module scoping — the caller's own file defines `name` at module level.
        // Multiple same-file defs (re-def): last one (by symbol id == parse order) wins.
        let same_file: Vec<&&(i64, Option<String>, String)> =
            mod_level.iter().filter(|(_, _, f)| f == call_site_file).collect();
        if let Some(last) = same_file.iter().max_by_key(|(id, _, _)| *id) {
            return Ok((Some(last.0), "exact"));
        }
        // T3(c): fall back to current behavior (all module-level, then full set)
        match mod_level.len() {
            1 => return Ok((Some(mod_level[0].0), "exact")),
            n if n > 1 => return Ok((None, "ambiguous")),
            _ => {} // only methods share the name; a bare call can't be them directly -> fallback
        }
    }

    Ok(match cands.len() {
        1 => (Some(cands[0].0), "exact"),
        _ => (None, "ambiguous"),
    })
}

/// T4 — single-hop inheritance: `rc` (already validated as a class with no own method `name`)
/// has `base_class=Some(B)`; if `B` uniquely names one class symbol and defines `name` exactly
/// once, that's exact. Anything else (no base, ambiguous base, base lacks it) -> None (fallback).
/// One hop only — B's own base is never consulted.
fn base_class_hop(
    conn: &Connection,
    rc: &str,
    cands: &[(i64, Option<String>, String)],
    lang: &str,
) -> Result<Option<(Option<i64>, &'static str)>> {
    let base: Option<String> = conn
        .prepare_cached("SELECT base_class FROM symbols WHERE name=?1 AND kind='class' AND lang=?2 LIMIT 1")?
        .query_row(params![rc, lang], |r| r.get(0))
        .optional()?
        .flatten();
    let Some(b) = base else { return Ok(None) };
    let mut cstmt = conn.prepare_cached("SELECT kind FROM symbols WHERE name=?1 AND lang=?2 LIMIT 2")?;
    let kinds: Vec<String> =
        cstmt.query_map(params![&b, lang], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
    if kinds.len() != 1 || kinds[0] != "class" {
        return Ok(None);
    }
    let filt: Vec<&(i64, Option<String>, String)> =
        cands.iter().filter(|(_, pc, _)| pc.as_deref() == Some(b.as_str())).collect();
    Ok((filt.len() == 1).then(|| (Some(filt[0].0), "exact")))
}

/// W2.1: conservative Python builtin types — a receiver hint naming one of these, with zero
/// in-repo symbols of the same name, is provably external.
const PY_BUILTINS: &[&str] = &[
    "dict", "list", "set", "frozenset", "tuple", "str", "bytes", "bytearray", "int", "float",
    "complex", "bool", "object", "type", "BaseException", "Exception",
];

/// W2.1: `rc` has zero in-repo symbols (checked by the caller); is it a builtin, or an import
/// whose source module doesn't match any in-repo file (T3 path-suffix matching; no match = external)?
fn is_external_receiver(conn: &Connection, rc: &str, call_site_file: &str) -> Result<bool> {
    if PY_BUILTINS.contains(&rc) {
        return Ok(true);
    }
    let module: Option<String> = conn
        .prepare_cached("SELECT source_module FROM import_names WHERE file=?1 AND local=?2 LIMIT 1")?
        .query_row(params![call_site_file, rc], |r| r.get(0))
        .optional()?;
    let Some(m) = module else { return Ok(false) };
    let suffix = format!("{}.py", m.replace('.', "/"));
    let exists: bool = conn
        .prepare_cached("SELECT 1 FROM files WHERE path=?1 OR path LIKE '%/' || ?1 LIMIT 1")?
        .query_row(params![suffix], |_| Ok(()))
        .optional()?
        .is_some();
    Ok(!exists)
}

fn row_to_symref(r: &rusqlite::Row) -> rusqlite::Result<(i64, SymRef)> {
    let name: String = r.get(1)?;
    let file: String = r.get(2)?;
    let parent_class: Option<String> = r.get(7)?;
    Ok((
        r.get::<_, i64>(0)?,
        SymRef {
            fq_name: fq_name(&file, &name, parent_class.as_deref()),
            name,
            file,
            start_line: r.get(3)?,
            end_line: r.get(4)?,
            signature: r.get(5)?,
            docstring: r.get(6)?,
        },
    ))
}

fn approx_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// W2.4 — pluggable token-counting seam. `approx` (chars/4) is the only impl today; future
/// per-model counters slot in here without touching the CLI or `Bundle` shape.
pub trait Tokenizer {
    fn count(&self, s: &str) -> usize;
    fn name(&self) -> &'static str;
}

pub struct ApproxTokenizer;

impl Tokenizer for ApproxTokenizer {
    fn count(&self, s: &str) -> usize {
        approx_tokens(s)
    }
    fn name(&self) -> &'static str {
        "approx"
    }
}

/// W2.2 — a caller is a test caller iff its call-site file has a `tests`/`test` path segment, or a
/// `test_*.py` / `*_test.py` basename. Computed at bundle time, not stored (no schema change).
fn is_test_file(path: &str) -> bool {
    let p = Path::new(path);
    if p.components().any(|c| matches!(c.as_os_str().to_str(), Some("tests") | Some("test"))) {
        return true;
    }
    match p.file_name().and_then(|f| f.to_str()) {
        Some(base) => base.ends_with(".py") && (base.starts_with("test_") || base.ends_with("_test.py")),
        None => false,
    }
}

fn read_span(path: &Path, start: i64, end: i64) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let s = ((start.max(1) as usize) - 1).min(lines.len());
    let e = (end.max(1) as usize).min(lines.len());
    if s >= e {
        return String::new();
    }
    lines[s..e].join("\n")
}

fn read_snippet(path: &Path, line: i64, radius: i64) -> String {
    read_span(path, line - radius, line + radius)
}

fn hash_bytes(b: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// L1.1 — every file under `root` (skip dirs excluded) whose extension is registered in
/// `parser::LANGS`. The generalized `python_files` walker.
fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn rec(dir: &Path, out: &mut Vec<PathBuf>) {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return,
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = e.file_name();
                if !SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                    rec(&p, out);
                }
            } else if crate::parser::lang_for_path(&p).is_some() {
                out.push(p);
            }
        }
    }
    rec(root, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// F2 — `Store::open` refuses a nonexistent repo path with a clear, actionable error instead of
    /// silently `create_dir_all`-ing `.maple` under whatever the path happens to resolve to.
    #[test]
    fn f2_open_rejects_nonexistent_repo_root() {
        let err = Store::open(Path::new("/definitely/does/not/exist/maple-f2-test")).err().unwrap();
        assert!(err.to_string().contains("repo path does not exist"), "{err}");
    }

    /// F2 — a repo path that exists but is a file (not a directory) is a clear config error.
    #[test]
    fn f2_open_rejects_repo_root_that_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not_a_repo.txt");
        fs::write(&file, b"hi").unwrap();
        let err = Store::open(&file).err().unwrap();
        assert!(err.to_string().contains("repo path is not a directory"), "{err}");
    }

    /// F2 — a `.maple` path that already exists as a FILE (not a directory) is a clear config error,
    /// not a confusing `create_dir_all` failure or a store that silently opens the wrong thing.
    #[test]
    fn f2_open_rejects_dot_maple_as_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join(".maple"), b"not a directory").unwrap();
        let err = Store::open(root).err().unwrap();
        assert!(err.to_string().contains(".maple exists but is not a directory"), "{err}");
    }

    /// F1 — CRITICAL regression guard: two `index_repo` calls racing on the SAME on-disk store must
    /// serialize (via `busy_timeout`) rather than error or interleave into a doubled/partial graph.
    /// Cold index now clears + inserts inside ONE transaction (see `index_repo`'s doc comment) —
    /// SQLite's own write-lock provides the serialization; this test proves the observable effect:
    /// whichever transaction commits last leaves a single, correct, non-doubled graph.
    #[test]
    fn f1_concurrent_index_runs_do_not_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        for i in 0..5 {
            fs::write(root.join(format!("m{i}.py")), format!("def f{i}():\n    return f{}()\n", (i + 1) % 5))
                .unwrap();
        }

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut s = Store::open(&root).unwrap();
                    barrier.wait(); // start both indexers as close to simultaneously as possible
                    s.index_repo(&root).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let s = Store::open(&root).unwrap();
        let (files, symbols, _imports) = s.counts().unwrap();
        let (edges, exact, ambiguous, unresolved) = s.edge_stats().unwrap();
        assert_eq!(files, 5, "not doubled: exactly one file row per source file");
        assert_eq!(symbols, 5, "not doubled: exactly one def per source file");
        assert_eq!(edges, 5, "not doubled: exactly one edge per call-site");
        assert_eq!(edges, exact + ambiguous + unresolved);
    }

    #[test]
    fn resolves_edges_and_upholds_d0() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def bar():\n    return 1\ndef solo():\n    return 2\n").unwrap();
        fs::write(root.join("c.py"), "def dup():\n    return 9\n").unwrap();
        // caller() makes 4 calls: baz()->bar (via alias, exact), solo() (exact),
        //                         dup() (b.py defines its own dup -> T3 module scoping, exact),
        //                         missing() (unresolved)
        fs::write(
            root.join("b.py"),
            "from a import bar as baz\ndef dup():\n    return 0\ndef caller():\n    baz()\n    solo()\n    dup()\n    missing()\n",
        )
        .unwrap();

        let mut s = Store::open(root).unwrap();
        let st = s.index_repo(root).unwrap();

        // D0: exactly one edge per call-site; nothing dropped. 4 calls total.
        assert_eq!(st.edges, 4, "one edge per call-site (D0)");
        assert_eq!(st.edges, st.exact + st.ambiguous + st.unresolved);
        assert_eq!(st.exact, 3, "baz->bar, solo, and dup (T3: caller's own file wins)");
        assert_eq!(st.ambiguous, 0, "dup() binds to b.py's own def, not c.py's (Python module scoping)");
        assert_eq!(st.unresolved, 1, "missing() not in repo");

        // alias binding: baz() resolved to bar's def (no under-inclusion silent gap)
        let (kind, has_sym): (String, bool) = s
            .conn
            .query_row(
                "SELECT kind, callee_symbol IS NOT NULL FROM edges WHERE callee_name='bar'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "exact");
        assert!(has_sym, "exact edge carries a resolved callee_symbol");

        // reload is a read
        drop(s);
        let s2 = Store::open(root).unwrap();
        assert_eq!(s2.edge_stats().unwrap().0, 4, "edges survived reopen");
    }

    #[test]
    fn closure_and_enumerate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def bar():\n    return 1\ndef solo():\n    return 2\n").unwrap();
        fs::write(root.join("c.py"), "def dup():\n    return 9\n").unwrap();
        fs::write(
            root.join("b.py"),
            "from a import bar as baz\ndef dup():\n    return 0\ndef caller():\n    baz()\n    solo()\n    dup()\n    missing()\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        // enumerate: dup defined twice, called once — T3 module scoping binds it exactly to
        // the caller's own file (b.py), not c.py's unrelated def.
        let e = s.enumerate("dup").unwrap();
        assert_eq!(e.def_count, 2);
        assert_eq!(e.caller_count, 1);
        assert_eq!(e.ambiguous, 0);
        assert_eq!(e.exact, 1);

        // closure of caller(): its 4 callees with correct resolutions
        let c = s.closure("caller", 1).unwrap();
        assert_eq!(c.callees.len(), 4);
        assert!(c.callees.iter().any(|x| x.name == "bar" && x.resolution == "exact"));
        assert!(c.callees.iter().any(|x| x.name == "dup" && x.resolution == "exact"));
        assert!(c.callees.iter().any(|x| x.name == "missing" && x.resolution == "unresolved"));
        assert!(c.callers.is_empty(), "nobody calls caller()");

        // closure of bar: one caller (caller(), via the baz alias)
        let cb = s.closure("bar", 1).unwrap();
        assert_eq!(cb.callers.len(), 1);
        assert_eq!(cb.callers[0].caller, "caller");
        assert_eq!(cb.targets.len(), 1);
    }

    #[test]
    fn bundle_assembles_caps_and_signals() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def bar():\n    return 1\ndef solo():\n    return 2\n").unwrap();
        fs::write(root.join("c.py"), "def dup():\n    return 9\n").unwrap();
        fs::write(
            root.join("b.py"),
            "from a import bar as baz\ndef dup():\n    return 0\ndef caller():\n    baz()\n    solo()\n    dup()\n    missing()\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        // full bundle for caller(): body + 4 callees + report flags. dup() binds exact to b.py's
        // own def (T3 module scoping) — not ambiguous, since this fixture shares b.py's own dup.
        let b = s.bundle("caller", 16000, 20, 1, 3, &ApproxTokenizer).unwrap();
        assert!(b.target.body.contains("def caller"));
        assert_eq!(b.callees.len(), 4);
        assert!(b.report.unresolved.contains(&"missing".to_string()));
        assert!(b.report.ambiguous.is_empty());
        assert!(b.report.token_count > 0 && !b.report.over_budget);
        assert_eq!(b.report.test_caller_count, 0, "no test callers in this fixture");

        // fan-in cap: bar has 1 caller; cap 0 -> 0 included, 1 omitted (reported, never silent)
        let capped = s.bundle("bar", 16000, 0, 1, 3, &ApproxTokenizer).unwrap();
        assert_eq!(capped.report.caller_count, 1);
        assert_eq!(capped.report.callers_included, 0);
        assert_eq!(capped.report.omitted.len(), 1);
        assert!(capped.target.body.contains("def bar"));

        // over_budget is a signal, not a trim: tiny budget still returns full bundle
        let small = s.bundle("caller", 5, 20, 1, 3, &ApproxTokenizer).unwrap();
        assert!(small.report.over_budget);
        assert_eq!(small.callees.len(), 4, "still complete despite over_budget (no silent trim)");

        // missing symbol -> clear error
        assert!(s.bundle("does_not_exist", 16000, 20, 1, 3, &ApproxTokenizer).is_err());
    }

    /// (edge kind, resolved callee's file) for the edge named `callee` whose caller is `encl`
    /// (enclosing fn/method name — fixtures below use globally-unique names).
    fn resolved(s: &Store, callee: &str, encl: &str) -> (String, Option<String>) {
        s.conn
            .query_row(
                "SELECT e.kind, sy.file FROM edges e \
                 LEFT JOIN symbols c ON e.caller_symbol=c.id \
                 LEFT JOIN symbols sy ON e.callee_symbol=sy.id \
                 WHERE e.callee_name=?1 AND c.name=?2",
                params![callee, encl],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    /// T1 — end-to-end: type-annotated params and a same-file return type narrow method calls to
    /// the exact def, even with 2 classes sharing the method name (would otherwise be ambiguous).
    #[test]
    fn t1_type_annotations_resolve_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("ab.py"),
            "class A:\n    def foo(self):\n        return 1\n\nclass B:\n    def foo(self):\n        return 2\n",
        )
        .unwrap();
        fs::write(
            root.join("use.py"),
            "def make() -> A:\n    return A()\n\
             def use_plain(a: A):\n    a.foo()\n\
             def use_optional(a: Optional[A]):\n    a.foo()\n\
             def use_ret():\n    x = make()\n    x.foo()\n\
             def use_list(a: list[A]):\n    a.foo()\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "foo", "use_plain"), ("exact".into(), Some("ab.py".into())));
        assert_eq!(resolved(&s, "foo", "use_optional"), ("exact".into(), Some("ab.py".into())));
        assert_eq!(resolved(&s, "foo", "use_ret"), ("exact".into(), Some("ab.py".into())), "same-file return type narrows");
        assert_eq!(resolved(&s, "foo", "use_list").0, "ambiguous", "list[A] carries no hint -> ambiguous (A.foo/B.foo)");
    }

    /// T2 — end-to-end: `self.attr = ClassName(...)` narrows `self.attr.method()` calls.
    #[test]
    fn t2_self_attr_resolves_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("ab.py"),
            "class A:\n    def foo(self):\n        return 1\n\nclass B:\n    def foo(self):\n        return 2\n",
        )
        .unwrap();
        fs::write(
            root.join("c.py"),
            "class C:\n    def __init__(self):\n        self.p = A()\n\n    def m(self):\n        self.p.foo()\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        assert_eq!(resolved(&s, "foo", "m"), ("exact".into(), Some("ab.py".into())));
    }

    /// T3 — end-to-end: import-aware `func` resolution prefers the caller's imported module, then
    /// its own module scope, before falling back to the blind global filter (ambiguous).
    #[test]
    fn t3_import_aware_func_resolves_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("m1.py"), "def dup():\n    return 1\n").unwrap();
        fs::write(root.join("m2.py"), "def dup():\n    return 2\n").unwrap();
        fs::write(root.join("via_import.py"), "from m1 import dup\ndef use_via_import():\n    dup()\n").unwrap();
        fs::write(root.join("via_local.py"), "def dup():\n    return 3\ndef use_via_local():\n    dup()\n").unwrap();
        fs::write(root.join("via_neither.py"), "def use_via_neither():\n    dup()\n").unwrap();

        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        assert_eq!(resolved(&s, "dup", "use_via_import"), ("exact".into(), Some("m1.py".into())));
        assert_eq!(resolved(&s, "dup", "use_via_local"), ("exact".into(), Some("via_local.py".into())));
        assert_eq!(resolved(&s, "dup", "use_via_neither").0, "ambiguous", "no import & no local def -> global fallback (3 defs)");
    }

    /// T4 — end-to-end: a subclass with no own method resolves through exactly one inheritance
    /// hop to its base's method; two-hop chains and multi-base classes are not narrowed.
    #[test]
    fn t4_single_hop_inheritance_resolves_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("base.py"),
            "class Base:\n    def foo(self):\n        return 1\n\nclass Other:\n    def foo(self):\n        return 2\n",
        )
        .unwrap();
        fs::write(
            root.join("sub.py"),
            "class C(Base):\n    pass\n\nclass Mid(Base):\n    pass\n\nclass Grand(Mid):\n    pass\n\nclass Multi(Base, Other):\n    pass\n",
        )
        .unwrap();
        fs::write(
            root.join("use.py"),
            "def use_c():\n    x = C()\n    x.foo()\n\n\
             def use_grand():\n    x = Grand()\n    x.foo()\n\n\
             def use_multi():\n    x = Multi()\n    x.foo()\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "foo", "use_c"), ("exact".into(), Some("base.py".into())), "one-hop C->Base.foo");
        assert_eq!(resolved(&s, "foo", "use_grand").0, "ambiguous", "two-hop Grand->Mid->Base not narrowed (one hop only)");
        assert_eq!(resolved(&s, "foo", "use_multi").0, "ambiguous", "multi-base -> base_class None, not narrowed");
    }

    /// F8 — named alias for `graph_projection`'s row shape (clippy::type_complexity).
    type ProjectionRows = Vec<(String, String, String, i64)>;

    /// id-free projection of the whole graph, for delta-vs-rebuild equivalence
    fn graph_projection(s: &Store) -> (ProjectionRows, ProjectionRows) {
        let mut syms: ProjectionRows = s
            .conn
            .prepare("SELECT file,name,kind,start_line FROM symbols")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        let mut edges: ProjectionRows = s
            .conn
            .prepare("SELECT callee_name,kind,call_site_file,call_site_line FROM edges")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        syms.sort();
        edges.sort();
        (syms, edges)
    }

    #[test]
    fn delta_refresh_equals_rebuild_and_relabels_unchanged_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // lib.py defines helper; app.py (will NOT change) calls helper()
        fs::write(root.join("lib.py"), "def helper():\n    return 1\n").unwrap();
        fs::write(root.join("app.py"), "def use():\n    return helper()\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        // baseline: app.py's call is exact
        let k: String = s
            .conn
            .query_row("SELECT kind FROM edges WHERE callee_name='helper' AND call_site_file='app.py'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(k, "exact");

        // no-op refresh: nothing changed
        let st0 = s.refresh().unwrap();
        assert_eq!((st0.changed, st0.deleted), (0, 0));

        // rename helper -> helper2 in lib.py ONLY; app.py untouched
        fs::write(root.join("lib.py"), "def helper2():\n    return 1\n").unwrap();
        let st = s.refresh().unwrap();
        assert_eq!(st.changed, 1, "only lib.py re-parsed");
        // the renamed-symbol case: app.py's edge (unchanged file) must be relabeled unresolved
        let k2: String = s
            .conn
            .query_row("SELECT kind FROM edges WHERE callee_name='helper' AND call_site_file='app.py'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(k2, "unresolved", "edge from UNCHANGED file relabeled after rename");

        // add a new file whose def resolves it back, plus a deletion
        fs::write(root.join("new.py"), "def helper():\n    return 3\n").unwrap();
        fs::write(root.join("gone.py"), "def tmp():\n    return 0\n").unwrap();
        s.refresh().unwrap();
        fs::remove_file(root.join("gone.py")).unwrap();
        let st2 = s.refresh().unwrap();
        assert_eq!(st2.deleted, 1);
        let k3: String = s
            .conn
            .query_row("SELECT kind FROM edges WHERE callee_name='helper' AND call_site_file='app.py'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(k3, "exact", "new def resolves the old call again");

        // --- extend: (i) an import-line edit (T3) and (ii) a base-class rename (T4) must both
        // stay delta==rebuild equivalent, and the rename must actually relabel the hop-dependent edge.
        fs::write(root.join("impbase1.py"), "def target():\n    return 1\n").unwrap();
        fs::write(root.join("impbase2.py"), "def target():\n    return 2\n").unwrap();
        fs::write(root.join("impcaller.py"), "from impbase1 import target\ndef use_import():\n    target()\n").unwrap();
        fs::write(
            root.join("basecls.py"),
            "class Base:\n    def m(self):\n        return 1\n\nclass Other:\n    def m(self):\n        return 2\n",
        )
        .unwrap();
        fs::write(root.join("subcls.py"), "class Sub(Base):\n    pass\n").unwrap();
        fs::write(root.join("usecls.py"), "def use_sub():\n    x = Sub()\n    x.m()\n").unwrap();
        s.refresh().unwrap();

        // (i) edit the import line: switch impcaller.py from impbase1 to impbase2
        let k_before: String = s
            .conn
            .query_row("SELECT kind FROM edges WHERE callee_name='target' AND call_site_file='impcaller.py'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(k_before, "exact", "T3: import-aware func resolution binds to impbase1");
        fs::write(root.join("impcaller.py"), "from impbase2 import target\ndef use_import():\n    target()\n").unwrap();
        s.refresh().unwrap();
        let (k_after, file_after): (String, Option<String>) = s
            .conn
            .query_row(
                "SELECT e.kind, sy.file FROM edges e LEFT JOIN symbols sy ON e.callee_symbol=sy.id \
                 WHERE e.callee_name='target' AND e.call_site_file='impcaller.py'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((k_after.as_str(), file_after.as_deref()), ("exact", Some("impbase2.py")), "import edit re-resolves to the new module");

        // (ii) rename a base class: Base -> Base2 in basecls.py ONLY; subcls.py/usecls.py untouched.
        // Pass-B hint: Base's own method name ("m") is deleted+reinserted (its file changed), so it
        // lands in the affected-names set regardless of the class rename itself — that's what
        // catches this T4-hop-dependent edge even though its OWN receiver_class ("Sub") didn't change.
        let k_sub_before: String = s
            .conn
            .query_row("SELECT kind FROM edges WHERE callee_name='m' AND call_site_file='usecls.py'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(k_sub_before, "exact", "T4: one-hop inheritance resolves Sub.m -> Base.m");
        fs::write(
            root.join("basecls.py"),
            "class Base2:\n    def m(self):\n        return 1\n\nclass Other:\n    def m(self):\n        return 2\n",
        )
        .unwrap();
        let st_rename = s.refresh().unwrap();
        assert_eq!(st_rename.changed, 1, "only basecls.py re-parsed");
        let k_sub_after: String = s
            .conn
            .query_row("SELECT kind FROM edges WHERE callee_name='m' AND call_site_file='usecls.py'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            k_sub_after, "ambiguous",
            "pass-B catches the T4 path: Sub's stale base_class ('Base') no longer validates post-rename, \
             so it correctly falls back to the universal answer instead of leaving a stale exact/dangling ref"
        );

        // EQUIVALENCE (guards A1 + D0): delta-maintained graph == from-scratch rebuild
        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "delta graph == full rebuild");
    }

    /// S2 — the Python exact resolver: type-aware narrowing of method calls + module-level
    /// filtering of bare calls. Never widens, never drops (D0).
    #[test]
    fn s2_python_resolver_narrows_method_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("a.py"),
            "class A:\n    def foo(self):\n        return 1\n    def go(self):\n        return self.foo()\n",
        )
        .unwrap();
        fs::write(root.join("b.py"), "class B:\n    def foo(self):\n        return 2\n").unwrap();
        fs::write(
            root.join("use.py"),
            "def run(x):\n    a = A()\n    a.foo()\n    A().foo()\n    x.foo()\n",
        )
        .unwrap();
        // bare-call filter fixture: one module-level `mk` + a method `mk`
        fs::write(root.join("m.py"), "def mk():\n    return 0\nclass M:\n    def mk(self):\n        return 1\ndef call_it():\n    return mk()\n").unwrap();

        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        fn kind_of(s: &Store, file: &str, line: i64) -> String {
            s.conn
                .query_row(
                    "SELECT kind FROM edges WHERE call_site_file=?1 AND call_site_line=?2 AND callee_name='foo'",
                    params![file, line],
                    |r| r.get(0),
                )
                .unwrap()
        }
        // self.foo() inside A.go -> exact to A.foo (was ambiguous: A.foo vs B.foo)
        assert_eq!(kind_of(&s, "a.py", 5), "exact", "self.foo() binds to enclosing class");
        // a = A(); a.foo() and A().foo() -> exact
        assert_eq!(kind_of(&s, "use.py", 3), "exact", "ctor-bound var narrows");
        assert_eq!(kind_of(&s, "use.py", 4), "exact", "direct ctor call narrows");
        // x.foo() with unknown receiver -> stays ambiguous (never guess)
        assert_eq!(kind_of(&s, "use.py", 5), "ambiguous", "unknown receiver stays ambiguous");
        // exact edges carry the right callee: A.foo lives in a.py
        let f: String = s
            .conn
            .query_row(
                "SELECT s.file FROM edges e JOIN symbols s ON e.callee_symbol=s.id \
                 WHERE e.call_site_file='use.py' AND e.call_site_line=3",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(f, "a.py");
        // bare mk() -> exact to the module-level def, not the method
        let (k, pc): (String, Option<String>) = s
            .conn
            .query_row(
                "SELECT e.kind, s.parent_class FROM edges e JOIN symbols s ON e.callee_symbol=s.id \
                 WHERE e.callee_name='mk' AND e.call_kind='func'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(k, "exact");
        assert!(pc.is_none(), "bare call bound to module-level def");

        // delta path: class rename in b.py -> nothing about A-bound edges changes; equivalence holds
        fs::write(root.join("b.py"), "class B2:\n    def foo(self):\n        return 2\n").unwrap();
        s.refresh().unwrap();
        assert_eq!(kind_of(&s, "a.py", 5), "exact", "still exact after unrelated class rename");
        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "S2 delta == rebuild");
    }

    /// F13 — target-spec forms address a specific def among same-named ones
    #[test]
    fn f13_target_spec_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/x.py"), "def foo():\n    return 1\n").unwrap();
        fs::write(root.join("pkg/y.py"), "def foo():\n    return 2\nclass K:\n    def foo(self):\n        return 3\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        // bare name -> all three defs (safe over-set)
        assert_eq!(s.closure("foo", 1).unwrap().targets.len(), 3);
        // file::name -> just that file's def
        let c = s.closure("pkg/x.py::foo", 1).unwrap();
        assert_eq!(c.targets.len(), 1);
        assert_eq!(c.targets[0].file, "pkg/x.py");
        // file:line -> innermost def containing the line (K.foo starts line 4)
        let c2 = s.closure("pkg/y.py:5", 1).unwrap();
        assert_eq!(c2.targets.len(), 1);
        assert_eq!(c2.targets[0].start_line, 4);
        // module.path.name -> pkg/y.py's module-level foo (T10.3: excludes K.foo, a method)
        let c3 = s.closure("pkg.y.foo", 1).unwrap();
        assert_eq!(c3.targets.len(), 1, "module-path form excludes methods (K.foo)");
        // Class.method fallback
        let c4 = s.closure("K.foo", 1).unwrap();
        assert_eq!(c4.targets.len(), 1);
        assert_eq!(c4.targets[0].start_line, 4);
    }

    /// F3 — `enumerate` honors the full target-spec grammar (routed through the same
    /// `lookup_targets` resolver as `closure`/`bundle`): a qualified spec resolves `defs` to the
    /// specific matched definition, while caller counts stay keyed on the resolved bare name.
    #[test]
    fn f3_enumerate_honors_target_spec_grammar() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/x.py"), "def foo():\n    return 1\n").unwrap();
        fs::write(
            root.join("pkg/y.py"),
            "def foo():\n    return 2\nclass K:\n    def foo(self):\n        return 3\n",
        )
        .unwrap();
        fs::write(root.join("use.py"), "from pkg.x import foo\ndef caller():\n    foo()\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        // bare name -> all 3 defs (unchanged safe over-set semantics)
        let bare = s.enumerate("foo").unwrap();
        assert_eq!(bare.def_count, 3);

        // file::name -> just that file's def; caller counts stay keyed on the resolved bare name
        let qualified = s.enumerate("pkg/x.py::foo").unwrap();
        assert_eq!(qualified.def_count, 1);
        assert_eq!(qualified.defs[0].file, "pkg/x.py");
        assert_eq!(qualified.symbol, "foo");
        assert_eq!(qualified.caller_count, 1, "use.py's import-bound call to pkg/x.py's foo");

        // Class.method fallback -> K.foo only
        let method = s.enumerate("K.foo").unwrap();
        assert_eq!(method.def_count, 1);
        assert_eq!(method.defs[0].start_line, 4);
    }

    /// N2 positive guard: every edge's label is in {exact, ambiguous, unresolved} — never a
    /// similarity/confidence score. Closure is graph-derived, never guessed.
    #[test]
    fn n2_label_domain_is_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def f():\n    g()\n    h()\ndef g():\n    return 1\ndef h():\n    return 2\ndef g_dup():\n    pass\n").unwrap();
        fs::write(root.join("b.py"), "class C:\n    def m(self):\n        return self.m()\nx = missing()\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        let labels: Vec<String> = s
            .conn
            .prepare("SELECT DISTINCT kind FROM edges")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        assert!(!labels.is_empty());
        for l in &labels {
            assert!(
                matches!(l.as_str(), "exact" | "ambiguous" | "unresolved"),
                "edge label outside the closed domain: {l}"
            );
        }
    }

    /// W2.1 — external-receiver reclassification: a method call whose receiver is *provably*
    /// external (a hardcoded builtin, or an import whose source module has no in-repo file) stops
    /// polluting `ambiguous` and becomes `unresolved` (D0: edge kept, only relabeled). Never guess:
    /// an in-repo symbol of ANY kind sharing the receiver name blocks the rule entirely.
    #[test]
    fn w21_external_receiver_reclassification() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // rc="Pool" is a genuine external import (source_module "external_sdk" has no in-repo
        // file); two unrelated in-repo `get` defs exist so the pre-W2.1 answer would be ambiguous.
        fs::write(
            root.join("ext.py"),
            "from external_sdk import Pool\n\
             class C:\n    def __init__(self):\n        self.p = Pool()\n    def use(self):\n        self.p.get()\n",
        )
        .unwrap();
        fs::write(root.join("other1.py"), "def get():\n    return 1\n").unwrap();
        fs::write(root.join("other2.py"), "class D:\n    def get(self):\n        return 2\n").unwrap();
        // counter-case: receiver hint names an in-repo FUNCTION (not class) -> precondition 1
        // (zero in-repo symbols named rc) is false -> rule must not apply, stays whatever it was.
        fs::write(
            root.join("counter.py"),
            "def Helper():\n    return None\n\
             class E:\n    def __init__(self):\n        self.h = Helper()\n    def use(self):\n        self.h.get()\n",
        )
        .unwrap();
        // builtin case: `x = dict(); x.get()` -> unresolved (dict is a hardcoded builtin)
        fs::write(root.join("builtin.py"), "def use_dict():\n    x = dict()\n    x.get()\n").unwrap();

        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        fn kind_for(s: &Store, file: &str) -> String {
            s.conn
                .query_row(
                    "SELECT kind FROM edges WHERE call_site_file=?1 AND callee_name='get'",
                    [file],
                    |r| r.get(0),
                )
                .unwrap()
        }
        assert_eq!(kind_for(&s, "ext.py"), "unresolved", "external import receiver -> unresolved, not ambiguous noise");
        assert_eq!(kind_for(&s, "counter.py"), "ambiguous", "receiver hint names an in-repo FUNCTION -> rule doesn't apply");
        assert_eq!(kind_for(&s, "builtin.py"), "unresolved", "dict is a hardcoded Python builtin -> unresolved");

        // D0 still holds: exact must not decrease, every edge labeled
        let (total, exact, ambiguous, unresolved) = s.edge_stats().unwrap();
        assert_eq!(total, exact + ambiguous + unresolved);

        // equivalence: edit the external import line (still external, different literal module) ->
        // refresh() must equal a fresh rebuild.
        fs::write(
            root.join("ext.py"),
            "from external_sdk2 import Pool\n\
             class C:\n    def __init__(self):\n        self.p = Pool()\n    def use(self):\n        self.p.get()\n",
        )
        .unwrap();
        s.refresh().unwrap();
        assert_eq!(kind_for(&s, "ext.py"), "unresolved", "still external after import-line edit");
        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "W2.1 delta == rebuild after external-import edit");

        // flip-back (pass-B): later add an in-repo class literally named "Pool" (no own get, no
        // base) -> precondition 1 now fails -> the edge relabels away from unresolved via pass-B,
        // landing back on the pre-W2.1 universal answer (ambiguous: 2 unrelated in-repo `get` defs).
        fs::write(root.join("poolclass.py"), "class Pool:\n    pass\n").unwrap();
        let st = s.refresh().unwrap();
        assert_eq!(st.changed, 1, "only the new file is parsed");
        assert_eq!(kind_for(&s, "ext.py"), "ambiguous", "flip-back: new in-repo symbol named Pool blocks the externality rule");

        let delta_proj2 = graph_projection(&s);
        let mut s3 = Store::open(root).unwrap();
        s3.index_repo(root).unwrap();
        assert_eq!(delta_proj2, graph_projection(&s3), "W2.1 delta == rebuild after flip-back");
    }

    /// W2.2 — test-aware bundles: a test caller is flagged `is_test`, sorted first, gets the full
    /// enclosing test-function body (beyond ±radius) instead of the ±radius snippet, and is never
    /// evicted from the caller cap in favor of a non-test caller.
    #[test]
    fn w22_test_aware_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.py"), "def target():\n    return 1\n").unwrap();
        // 3 non-test callers, each on their own line, spread across a file (so ±3 stays cheap)
        fs::write(
            root.join("app.py"),
            "def n1():\n    target()\n\n\n\n\ndef n2():\n    target()\n\n\n\n\ndef n3():\n    target()\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        // the call sits 10 lines above an assert -> beyond a ±3 snippet, inside the full test body
        fs::write(
            root.join("tests/test_lib.py"),
            "def test_it():\n    x = target()\n    a = 1\n    a = 1\n    a = 1\n    a = 1\n    a = 1\n    \
             a = 1\n    a = 1\n    a = 1\n    assert x == 1\n",
        )
        .unwrap();

        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        // cap smaller than total fan-in (1 test + 3 non-test = 4 callers, cap 1): the test caller
        // must still be the one included — never evicted in favor of a non-test one.
        let b = s.bundle("target", 16000, 1, 1, 3, &ApproxTokenizer).unwrap();
        assert_eq!(b.report.caller_count, 4);
        assert_eq!(b.report.test_caller_count, 1);
        assert_eq!(b.report.callers_included, 1);
        assert_eq!(b.callers.len(), 1);
        assert!(b.callers[0].is_test, "cap never evicts the test caller for a non-test one");
        assert!(b.callers[0].call_site.file.ends_with("test_lib.py"));
        assert!(b.callers[0].call_site.snippet.contains("assert x == 1"), "full test body reaches the assert, beyond ±3");

        // full bundle: tests sort first, non-test snippet stays ±3 (doesn't reach the assert/body)
        let full = s.bundle("target", 16000, 20, 1, 3, &ApproxTokenizer).unwrap();
        assert_eq!(full.callers.len(), 4);
        assert!(full.callers[0].is_test, "test caller sorts first");
        assert!(full.callers[1..].iter().all(|c| !c.is_test));
        let non_test = full.callers.iter().find(|c| !c.is_test).unwrap();
        assert!(non_test.call_site.snippet.contains("def n1") || non_test.call_site.snippet.contains("def n2") || non_test.call_site.snippet.contains("def n3"));
        assert!(!non_test.call_site.snippet.contains("assert"), "non-test callers keep the narrow ±3 snippet");

        // regression guard: a qualified F13 spec ("file.py::name") must resolve the full test body
        // via the same-named-but-different `cl.symbol` (the bare resolved name edges are keyed on,
        // not the raw spec string) — this once silently fell back to ±3 for every qualified lookup.
        let qualified = s.bundle("lib.py::target", 16000, 20, 1, 3, &ApproxTokenizer).unwrap();
        let test_caller = qualified.callers.iter().find(|c| c.is_test).unwrap();
        assert!(test_caller.call_site.snippet.contains("assert x == 1"), "qualified spec still reaches the full test body");
    }

    /// T10.1 — fq_name = `<module path>.<name>`: a leading `src/` segment is stripped, other paths
    /// keep their full slash->dot form; computed at query time (no schema change).
    #[test]
    fn t10_fq_name_correctness() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src/pg_raggraph")).unwrap();
        fs::write(root.join("src/pg_raggraph/config.py"), "def load():\n    return 1\n").unwrap();
        fs::write(root.join("plain.py"), "def top():\n    return 1\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        let e1 = s.enumerate("load").unwrap();
        assert_eq!(e1.defs[0].fq_name, "pg_raggraph.config.load", "leading src/ segment stripped");

        let e2 = s.enumerate("top").unwrap();
        assert_eq!(e2.defs[0].fq_name, "plain.top", "no src/ prefix: path (minus .py) + name");
    }

    /// F4 — a method's fq_name includes its class: `pkg.mod.Widget.render`, not `pkg.mod.render`
    /// (which would collide with an unrelated module-level `render`). Display-only — resolution
    /// stays name-based (checked elsewhere; this only asserts the rendered fq_name string).
    #[test]
    fn f4_method_fq_name_includes_class() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(
            root.join("pkg/mod.py"),
            "class Widget:\n    def render(self):\n        return 1\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        let e = s.enumerate("render").unwrap();
        assert_eq!(e.defs[0].fq_name, "pkg.mod.Widget.render", "method fq_name includes the class segment");

        // bundle's target and exists() both go through the same fq_name computation
        let b = s.bundle("render", 16000, 20, 1, 3, &ApproxTokenizer).unwrap();
        assert_eq!(b.target.fq_name, "pkg.mod.Widget.render");
        let ex = s.exists("render", false).unwrap();
        assert_eq!(ex.defs[0].fq_name, "pkg.mod.Widget.render");
    }

    /// T10.2 — docstring flows parser -> symbols column -> bundle target AND callee entries.
    #[test]
    fn t10_docstring_in_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("lib.py"),
            "def documented():\n    \"\"\"One line only.\"\"\"\n    return 1\n\n\
             def target():\n    \"\"\"Compute the answer.\n\n    Longer body.\n    \"\"\"\n    return documented()\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        let b = s.bundle("target", 16000, 20, 1, 3, &ApproxTokenizer).unwrap();
        assert_eq!(b.target.docstring.as_deref(), Some("Compute the answer."), "target: first line only");
        let callee = b.callees.iter().find(|c| c.name == "documented").unwrap();
        assert_eq!(callee.docstring.as_deref(), Some("One line only."), "callee entries carry it too");
    }

    /// T9 — blast radius from a parsed diff: an edited function's span is the only changed_symbols
    /// entry (with its caller); an edit outside any def/class span lands in files_no_symbols.
    #[test]
    fn t9_impact_blast_radius_from_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.py"), "def target():\n    return 1\nCONST = 1\n").unwrap();
        fs::write(root.join("app.py"), "def caller():\n    return target()\n").unwrap();
        git(root, &["init"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);

        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        // edit target()'s body -> its span is touched
        fs::write(root.join("lib.py"), "def target():\n    return 2\nCONST = 1\n").unwrap();
        let diff = run_git_diff(root.to_str().unwrap(), Some("HEAD"), false).unwrap();
        let (changed, deleted) = parse_diff(&diff);
        assert!(deleted.is_empty());
        let impact = s.impact(&changed, &deleted).unwrap();
        assert_eq!(impact.changed_symbols.len(), 1, "only target() touched");
        let sym = &impact.changed_symbols[0];
        assert_eq!(sym.fq_name, "lib.target");
        assert_eq!(sym.status, "changed");
        assert_eq!(sym.caller_count, 1);
        assert_eq!(sym.callers[0].caller, "caller");
        assert!(impact.files_no_symbols.is_empty());
        drop(s);

        // reset to the committed baseline, then edit CONST (outside any symbol's span)
        git(root, &["checkout", "--", "lib.py"]);
        fs::write(root.join("lib.py"), "def target():\n    return 1\nCONST = 2\n").unwrap();
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        let diff2 = run_git_diff(root.to_str().unwrap(), Some("HEAD"), false).unwrap();
        let (changed2, deleted2) = parse_diff(&diff2);
        let impact2 = s2.impact(&changed2, &deleted2).unwrap();
        assert!(impact2.changed_symbols.is_empty(), "CONST is outside any def/class span");
        assert!(impact2.files_no_symbols.contains(&"lib.py".to_string()));
    }

    /// T15 — parse-failure visibility. Tree-sitter is error-tolerant (see the doc caveat in
    /// WAVE-3.5-SPEC.md): feeding it garbage doesn't return `Err` from `parse_python` — it returns
    /// a tree with zero recognized defs/calls/imports. That's the failure mode actually triggerable
    /// here: "suspect" (non-empty content, zero defs+calls+imports). Recorded by `index_repo`,
    /// surfaced in `enumerate`/`bundle`, cleared by `refresh()` once fixed, and delta==rebuild
    /// equivalence (including the `parse_failures` table itself) holds across that transition.
    #[test]
    fn t15_suspect_file_recorded_surfaced_and_cleared_on_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("good.py"), "def helper():\n    return 1\n").unwrap();
        // non-empty, not valid Python, but tree-sitter still returns Ok(tree) — zero defs/calls/imports
        fs::write(root.join("broken.py"), "@!#$ not python at all {{{ ]] (((\n").unwrap();

        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(s.parse_failures().unwrap(), vec!["broken.py".to_string()], "cold index flags the suspect file");

        let e = s.enumerate("helper").unwrap();
        assert_eq!(e.unparsed_files_count, 1, "repo-wide count, independent of the queried symbol");

        let b = s.bundle("helper", 16000, 20, 1, 3, &ApproxTokenizer).unwrap();
        assert_eq!(b.report.unparsed_files_count, 1);
        assert_eq!(b.report.unparsed_files, vec!["broken.py".to_string()]);

        // fix it; refresh() clears the flag
        fs::write(root.join("broken.py"), "def fixed():\n    return 2\n").unwrap();
        let st = s.refresh().unwrap();
        assert_eq!(st.changed, 1);
        assert!(s.parse_failures().unwrap().is_empty(), "fixed file no longer flagged");
        assert_eq!(s.enumerate("helper").unwrap().unparsed_files_count, 0);

        // delta == rebuild equivalence across the broken->fixed transition, including parse_failures
        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "T15: delta graph == rebuild after broken->fixed");
        assert_eq!(s.parse_failures().unwrap(), s2.parse_failures().unwrap(), "parse_failures also equivalent");
    }

    /// T15 — the genuine "unreadable" failure mode (permission-denied), the one path tree-sitter's
    /// error-tolerance can't paper over. Unix-only; self-skips under root (chmod 0 is not enforced
    /// for root, e.g. some CI containers) rather than asserting a false failure.
    #[test]
    #[cfg(unix)]
    fn t15_unreadable_file_is_recorded() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let path = root.join("locked.py");
        fs::write(&path, "def x():\n    return 1\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&path, perms).unwrap();

        // ponytail: no permission enforcement (root, or a fs that ignores mode bits) -> nothing to
        // assert; restore and bail rather than asserting a false failure.
        if std::fs::read(&path).is_ok() {
            let mut restore = fs::metadata(&path).unwrap().permissions();
            restore.set_mode(0o644);
            fs::set_permissions(&path, restore).unwrap();
            return;
        }

        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        let failures = s.parse_failures().unwrap();
        assert_eq!(failures, vec!["locked.py".to_string()]);

        let mut restore = fs::metadata(&path).unwrap().permissions();
        restore.set_mode(0o644);
        fs::set_permissions(&path, restore).unwrap();
    }

    /// T16 — `exists`: duplicate-prevention query. Exact name match by default (case-sensitive,
    /// no fuzzy matching — N2); `--prefix` widens to a deterministic prefix match.
    #[test]
    fn t16_exists_duplicate_prevention_query() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def embed():\n    return 1\n").unwrap();
        fs::write(root.join("b.py"), "def embed():\n    return 2\ndef embed_batch():\n    return 3\n").unwrap();
        fs::write(root.join("c.py"), "from a import embed\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        let e = s.exists("embed", false).unwrap();
        assert_eq!(e.defs.len(), 2, "two defs named exactly 'embed'");
        assert!(e.defs.iter().all(|d| d.kind == "function"));
        assert_eq!(e.import_names_matches.len(), 1);
        assert_eq!(e.import_names_matches[0].source_module, "a");

        let none = s.exists("brand_new_name", false).unwrap();
        assert!(none.defs.is_empty(), "empty defs -> safe to create");
        assert!(none.import_names_matches.is_empty());

        let p = s.exists("embed", true).unwrap();
        assert_eq!(p.defs.len(), 3, "embed x2 + embed_batch");
        assert!(p.defs.iter().any(|d| d.fq_name.ends_with("embed_batch")));

        // case-sensitive, never guesses
        fs::write(root.join("d.py"), "def Embed():\n    return 4\n").unwrap();
        s.refresh().unwrap();
        assert_eq!(s.exists("embed", false).unwrap().defs.len(), 2, "'Embed' does not match 'embed'");
    }

    /// T16 — `surface`: the API surface of a module an agent is about to extend — module-level
    /// defs plus classes with their methods nested (not flattened), raw imports, and an `unparsed`
    /// flag. Accepts a file path or a dotted module path; a not-yet-created module is an empty
    /// surface, not an error (Day-0 friendly).
    #[test]
    fn t16_surface_lists_module_api() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(
            root.join("pkg/mod.py"),
            "import os\nfrom pkg.other import thing\n\ndef top():\n    return 1\n\n\
             class Widget:\n    def a(self):\n        return 1\n    def b(self):\n        return 2\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        let surf = s.surface("pkg/mod.py").unwrap();
        assert_eq!(surf.defs.len(), 2, "top() + Widget, methods nested not flattened");
        let widget = surf.defs.iter().find(|d| d.name == "Widget").unwrap();
        assert_eq!(widget.methods.len(), 2);
        assert!(widget.methods.iter().any(|m| m.name == "a"));
        assert_eq!(surf.imports.len(), 2);
        assert!(!surf.unparsed);

        // dotted module path resolves the same file
        let surf2 = s.surface("pkg.mod").unwrap();
        assert_eq!(surf2.defs.len(), 2);

        // a suspect (T15) module surfaces unparsed: true
        fs::write(root.join("pkg/broken.py"), "@!#$ {{{ not python\n").unwrap();
        s.refresh().unwrap();
        let surf3 = s.surface("pkg/broken.py").unwrap();
        assert!(surf3.unparsed);
        assert!(surf3.defs.is_empty());

        // a module that doesn't exist yet -> empty surface, not an error
        let surf4 = s.surface("pkg/new_thing.py").unwrap();
        assert!(surf4.defs.is_empty() && surf4.imports.is_empty() && !surf4.unparsed);
    }

    /// T17 — worktree warm-start: seeding a copy of A's index into B, then refreshing against B's
    /// (slightly different) tree, reports the right delta and lands on the same graph a fresh cold
    /// index of that tree would produce. Also: refuses an existing target index without `--force`,
    /// and errors clearly when the source has no index at all.
    #[test]
    fn t17_seed_warm_starts_a_worktree() {
        let tmp_a = tempfile::tempdir().unwrap();
        let a = tmp_a.path();
        fs::write(a.join("lib.py"), "def helper():\n    return 1\n").unwrap();
        fs::write(a.join("app.py"), "def use():\n    return helper()\n").unwrap();
        let mut sa = Store::open(a).unwrap();
        sa.index_repo(a).unwrap();
        drop(sa);

        // B: same tree as A, except app.py picked up one extra line (a worktree with local edits)
        let tmp_b = tempfile::tempdir().unwrap();
        let b = tmp_b.path();
        fs::write(b.join("lib.py"), "def helper():\n    return 1\n").unwrap();
        fs::write(b.join("app.py"), "def use():\n    x = helper()\n    return x\n").unwrap();

        let stats = seed(b, a, false).unwrap();
        assert_eq!(stats.changed, 1, "only app.py differs from the seeded snapshot");

        let sb = Store::open(b).unwrap();
        let seeded_proj = graph_projection(&sb);

        // a fresh cold index of the SAME tree (content-identical, different tempdir root — the
        // projection is path-relative) must land on the same graph
        let tmp_c = tempfile::tempdir().unwrap();
        let c = tmp_c.path();
        fs::write(c.join("lib.py"), "def helper():\n    return 1\n").unwrap();
        fs::write(c.join("app.py"), "def use():\n    x = helper()\n    return x\n").unwrap();
        let mut sc = Store::open(c).unwrap();
        sc.index_repo(c).unwrap();
        assert_eq!(seeded_proj, graph_projection(&sc), "seeded+refreshed graph == fresh index of the same tree");
        drop(sb);

        // refuses to clobber an existing target index without --force
        let err = seed(b, a, false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        // --force overwrites it
        assert!(seed(b, a, true).is_ok());

        // a source with no index at all is a clear error
        let tmp_d = tempfile::tempdir().unwrap();
        let tmp_e = tempfile::tempdir().unwrap();
        let err2 = seed(tmp_e.path(), tmp_d.path(), false).unwrap_err();
        assert!(err2.to_string().contains("no index"), "{err2}");
    }

    /// T18 — greenfield lifecycle: a Day-0 repo (one near-empty `.py` file) indexes cleanly; adding
    /// a new file with new symbols is picked up by the next `refresh()` with no re-index; a
    /// zero-caller symbol is a valid, honest `bundle` result (not an error); a new file calling an
    /// existing symbol produces a real edge that shows up in that symbol's closure.
    #[test]
    fn t18_greenfield_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // (a) index a repo with ONE nearly-empty .py file
        fs::write(root.join("main.py"), "# just a comment, nothing else\n").unwrap();
        let mut s = Store::open(root).unwrap();
        let st = s.index_repo(root).unwrap();
        assert_eq!(st.files, 1);
        assert_eq!(st.symbols, 0);
        assert_eq!(s.counts().unwrap(), (1, 0, 0), "status sane on a near-empty repo");

        // (b) add a new file with new symbols; no re-index — refresh() picks it up
        fs::write(root.join("greeter.py"), "def greet():\n    return 'hi'\n").unwrap();
        s.refresh().unwrap();
        let e = s.enumerate("greet").unwrap();
        assert_eq!(e.def_count, 1, "refresh() picked up the new file's def with no re-index");

        // 0-caller symbol: bundle returns callers=[] / caller_count=0 — valid, honest, not an error
        let b = s.bundle("greet", 16000, 20, 1, 3, &ApproxTokenizer).unwrap();
        assert_eq!(b.report.caller_count, 0);
        assert!(b.callers.is_empty());

        // (c) a new file calling the old symbol -> edge appears; old symbol's closure gains the caller
        fs::write(root.join("caller.py"), "from greeter import greet\ndef run():\n    return greet()\n").unwrap();
        let rst = s.refresh().unwrap();
        assert_eq!(rst.changed, 1, "only the new file is parsed");
        let cl = s.closure("greet", 1).unwrap();
        assert_eq!(cl.callers.len(), 1);
        assert_eq!(cl.callers[0].caller, "run");

        // equivalence holds across this whole greenfield growth sequence
        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "greenfield growth: delta == rebuild");
    }

    /// T11 — a schema-version mismatch (simulated via a PRAGMA downgrade, standing in for a db
    /// written by a pre-T11 binary — see the Wave 4 gate) resets to an empty store on `open()`
    /// instead of ever raising a raw SQL error, and `refresh()` on that just-reset empty store
    /// behaves exactly like a cold walk: queries afterward return correct data, and the graph it
    /// produces equals a fresh `index_repo`.
    #[test]
    fn t11_schema_version_mismatch_triggers_auto_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def foo():\n    return 1\n").unwrap();
        fs::write(root.join("b.py"), "def bar():\n    return foo()\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        assert_eq!(s.counts().unwrap(), (2, 2, 0));
        drop(s);

        // simulate a stale db written by a different maple schema version
        {
            let conn = Connection::open(root.join(".maple/graph.db")).unwrap();
            conn.pragma_update(None, "user_version", 999i32).unwrap();
        }

        // reopen: auto-reset, no raw SQL error, store is empty (not stale garbage)
        let mut s2 = Store::open(root).unwrap();
        assert_eq!(s2.counts().unwrap(), (0, 0, 0), "version mismatch resets to an empty store");

        // refresh() on the just-reset empty store behaves like a cold walk
        let st = s2.refresh().unwrap();
        assert_eq!(st.changed, 2, "every on-disk file looks 'changed' against an empty store");
        assert_eq!(s2.counts().unwrap(), (2, 2, 0), "refresh after reset re-populates from disk");
        let e = s2.enumerate("foo").unwrap();
        assert_eq!(e.def_count, 1);
        assert_eq!(e.caller_count, 1, "the bar->foo edge survives the reset+refresh round trip");

        // equivalence: reset-then-refresh graph == a fresh cold index of the same tree
        let delta_proj = graph_projection(&s2);
        let mut s3 = Store::open(root).unwrap();
        s3.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s3), "schema-reset refresh == rebuild");

        // reopening again now matches SCHEMA_VERSION -> no reset, data survives
        let s4 = Store::open(root).unwrap();
        assert_eq!(s4.counts().unwrap(), (2, 2, 0), "no spurious reset once the version matches");
    }

    /// T12 — git-aware delta: an uncommitted edit is detected via `git status --porcelain`
    /// candidates alone (the fast path), without a full repo hash walk, and lands on the same
    /// graph a full rebuild would.
    #[test]
    fn t12_git_delta_detects_uncommitted_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def foo():\n    return 1\n").unwrap();
        fs::write(root.join("b.py"), "def bar():\n    return foo()\n").unwrap();
        git(root, &["init"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);

        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        // no-op refresh right after a cold index: HEAD matches, porcelain is empty -> nothing changed
        let st0 = s.refresh().unwrap();
        assert_eq!((st0.changed, st0.deleted), (0, 0));

        // modify a.py without committing
        fs::write(root.join("a.py"), "def foo():\n    return 2\n").unwrap();
        let st = s.refresh().unwrap();
        assert_eq!(st.changed, 1, "only a.py reparsed");
        assert_eq!(st.deleted, 0);

        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "git-aware delta == rebuild");
    }

    /// T12 — a commit between refreshes moves HEAD, and the edit that just got committed leaves
    /// `git status --porcelain` completely empty (working tree == new HEAD): trusting porcelain
    /// alone here would wrongly report "nothing changed" even though the store is still one commit
    /// behind. A HEAD change must fall back to the full hash walk once to catch that — correctness
    /// over speed for this one call — then the fast path resumes on the next refresh.
    #[test]
    fn t12_git_delta_falls_back_on_head_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def foo():\n    return 1\n").unwrap();
        git(root, &["init"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();
        // sync last_indexed_head to this commit (a no-op refresh right after the cold index)
        let st0 = s.refresh().unwrap();
        assert_eq!((st0.changed, st0.deleted), (0, 0));

        // edit + commit: HEAD moves, and the working tree now matches the new HEAD exactly, so
        // porcelain alone reports nothing — the store is nonetheless one commit stale.
        fs::write(root.join("a.py"), "def foo():\n    return 2\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "second"]);

        let st = s.refresh().unwrap();
        assert_eq!(st.changed, 1, "HEAD moved; the full-walk fallback still finds the real change porcelain alone would miss");

        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "HEAD-change fallback delta == rebuild");

        // fast path resumes on the next refresh (HEAD is stable again, only porcelain candidates checked)
        fs::write(root.join("b.py"), "def bar():\n    return 1\n").unwrap();
        let st2 = s.refresh().unwrap();
        assert_eq!(st2.changed, 1);

        let delta_proj2 = graph_projection(&s);
        let mut s3 = Store::open(root).unwrap();
        s3.index_repo(root).unwrap();
        assert_eq!(delta_proj2, graph_projection(&s3), "resumed fast path delta == rebuild");
    }

    /// T12 — an uncommitted deletion is detected: the porcelain candidate no longer exists on disk,
    /// and was previously indexed, so it's classified `deleted` without a full-tree walk.
    #[test]
    fn t12_git_delta_detects_uncommitted_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def foo():\n    return 1\n").unwrap();
        fs::write(root.join("gone.py"), "def tmp():\n    return 0\n").unwrap();
        git(root, &["init"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        fs::remove_file(root.join("gone.py")).unwrap();
        let st = s.refresh().unwrap();
        assert_eq!(st.deleted, 1, "uncommitted deletion detected via the porcelain candidate set");
        assert_eq!(st.changed, 0);

        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "deletion delta == rebuild");
    }

    /// T13 — rayon parallelizes the parse step (file read + tree-sitter), but every SQLite write
    /// stays single-threaded and in original file order: repeatedly cold-indexing the same
    /// multi-file fixture must land on byte-for-byte the same graph every time (no data race, no
    /// nondeterministic reordering leaking into the persisted result).
    #[test]
    fn t13_parallel_parse_is_deterministic_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for i in 0..8 {
            fs::write(
                root.join(format!("m{i}.py")),
                format!("def f{i}():\n    return f{}()\n", (i + 1) % 8),
            )
            .unwrap();
        }
        let mut s = Store::open(root).unwrap();
        let mut projections = Vec::new();
        for _ in 0..3 {
            s.index_repo(root).unwrap();
            projections.push(graph_projection(&s));
        }
        assert_eq!(projections[0], projections[1], "parallel cold-index run 1 == run 2");
        assert_eq!(projections[0], projections[2], "parallel cold-index run 1 == run 3");
        assert_eq!(projections[0].0.len(), 8, "sanity: all 8 defs present");
        assert_eq!(projections[0].1.len(), 8, "sanity: all 8 calls resolved to edges");
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=test"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    // ---- L1.4 — per-language universal-tier fixtures ---------------------------------------

    /// (edge kind, call_kind) for the edge named `callee` whose enclosing caller is `encl`.
    fn edge_kinds(s: &Store, callee: &str, encl: &str) -> (String, String) {
        s.conn
            .query_row(
                "SELECT e.kind, e.call_kind FROM edges e LEFT JOIN symbols c ON e.caller_symbol=c.id \
                 WHERE e.callee_name=?1 AND c.name=?2",
                params![callee, encl],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn parent_of(s: &Store, name: &str) -> Option<String> {
        s.conn
            .query_row("SELECT parent_class FROM symbols WHERE name=?1", [name], |r| r.get(0))
            .unwrap()
    }

    /// L1.4 Rust — cross-file func call exact; `use .. as` alias binds; method call lands kind
    /// `method` with the free `self`-in-`impl` receiver hint; defs carry the impl-type container.
    #[test]
    fn l1_rust_fixture_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("lib.rs"),
            "pub fn helper() -> i32 { 1 }\n\
             pub fn plain() -> i32 { 2 }\n\
             pub struct Widget { pub val: i32 }\n\
             impl Widget {\n    pub fn render(&self) -> i32 { self.helper_method() }\n    fn helper_method(&self) -> i32 { helper() }\n}\n",
        )
        .unwrap();
        fs::write(root.join("app.rs"), "use crate::lib::helper as h;\nfn run() { h(); plain(); }\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "plain", "run"), ("exact".into(), Some("lib.rs".into())), "cross-file func call");
        assert_eq!(resolved(&s, "helper", "run"), ("exact".into(), Some("lib.rs".into())), "use-as alias binds");
        assert_eq!(edge_kinds(&s, "helper_method", "render"), ("exact".into(), "method".into()), "self.x() -> method + impl-type hint");
        assert_eq!(parent_of(&s, "render").as_deref(), Some("Widget"), "method def carries impl type");
        let widget_kind: String =
            s.conn.query_row("SELECT kind FROM symbols WHERE name='Widget'", [], |r| r.get(0)).unwrap();
        assert_eq!(widget_kind, "class", "struct is a class-kind container");
    }

    /// L1.4 Go — cross-file func call exact; `w.foo()` on the method's own receiver ident gets the
    /// receiver-type hint and lands kind `method`; defs carry the receiver type as parent.
    #[test]
    fn l1_go_fixture_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("lib.go"),
            "package main\n\nfunc Helper() int { return 1 }\n\ntype Widget struct{ Val int }\n\n\
             func (w *Widget) Render() int { return w.helperMethod() }\n\n\
             func (w *Widget) helperMethod() int { return Helper() }\n",
        )
        .unwrap();
        fs::write(root.join("app.go"), "package main\n\nfunc Run() int { return Helper() }\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "Helper", "Run"), ("exact".into(), Some("lib.go".into())), "cross-file func call");
        assert_eq!(edge_kinds(&s, "helperMethod", "Render"), ("exact".into(), "method".into()), "receiver ident -> hint -> exact");
        assert_eq!(parent_of(&s, "Render").as_deref(), Some("Widget"), "method def carries receiver type");
    }

    /// L1.4 C — cross-file func call exact; C has no methods: `s->fn()` stays kind `func`
    /// (honest `unresolved` when nothing defines it); #include recorded as a raw import.
    #[test]
    fn l1_c_fixture_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.c"), "int helper(int x) { return x + 1; }\n").unwrap();
        fs::write(
            root.join("app.c"),
            "#include \"lib.h\"\nint run(void) { return helper(1); }\n\
             int use_fp(struct S* s) { return s->fn(2); }\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "helper", "run"), ("exact".into(), Some("lib.c".into())), "cross-file func call");
        assert_eq!(edge_kinds(&s, "fn", "use_fp"), ("unresolved".into(), "func".into()), "C member call stays func");
        let imports: i64 =
            s.conn.query_row("SELECT COUNT(*) FROM imports WHERE file='app.c'", [], |r| r.get(0)).unwrap();
        assert_eq!(imports, 1, "#include recorded as raw import");
    }

    /// L1.4 C++ — cross-file func call exact; member call lands kind `method`; class-body methods
    /// AND out-of-line `X::y` definitions both carry the class as parent.
    #[test]
    fn l1_cpp_fixture_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("lib.cpp"),
            "int helper(int x) { return x + 1; }\n\
             class Widget {\npublic:\n    int go() { return helper_method(); }\n    int helper_method() { return 1; }\n};\n\
             int Widget::extra() { return 42; }\n",
        )
        .unwrap();
        fs::write(root.join("app.cpp"), "int run() { return helper(1); }\nint use(Widget& w) { return w.go(); }\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "helper", "run"), ("exact".into(), Some("lib.cpp".into())), "cross-file func call");
        assert_eq!(edge_kinds(&s, "go", "use"), ("exact".into(), "method".into()), "member call -> method");
        assert_eq!(parent_of(&s, "go").as_deref(), Some("Widget"), "in-class method carries class");
        assert_eq!(parent_of(&s, "extra").as_deref(), Some("Widget"), "out-of-line X::y carries class");
    }

    /// L1.4 C# — cross-file func-kind call exact; member call lands kind `method`; methods carry
    /// their class.
    #[test]
    fn l1_csharp_fixture_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("Lib.cs"),
            "class Widget {\n    public int Go() { return HelperMethod(); }\n    public int HelperMethod() { return 1; }\n}\n\
             class Util {\n    public static int Helper(int x) { return x + 1; }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("App.cs"),
            "class Program {\n    static int Run() { return Helper(1); }\n    static void Use(Widget w) { w.Go(); }\n}\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "Helper", "Run"), ("exact".into(), Some("Lib.cs".into())), "cross-file func call");
        assert_eq!(edge_kinds(&s, "Go", "Use"), ("exact".into(), "method".into()), "member call -> method");
        assert_eq!(parent_of(&s, "Go").as_deref(), Some("Widget"), "method def carries class");
    }

    /// L1.4 Java — cross-file func-kind call exact; `x.foo()` lands kind `method`; methods carry
    /// their class; imports record the last segment (import_names).
    #[test]
    fn l1_java_fixture_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("Lib.java"),
            "class Widget {\n    int go() { return helperMethod(); }\n    int helperMethod() { return 1; }\n}\n\
             class MathUtil {\n    static int compute(int x) { return x + 1; }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("App.java"),
            "import util.MathUtil;\nclass App {\n    int run(Widget w) { return w.go(); }\n    int calc() { return compute(1); }\n}\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "compute", "calc"), ("exact".into(), Some("Lib.java".into())), "cross-file func call");
        assert_eq!(edge_kinds(&s, "go", "run"), ("exact".into(), "method".into()), "x.foo() -> method");
        assert_eq!(parent_of(&s, "go").as_deref(), Some("Widget"), "method def carries class");
        let (local, module): (String, String) = s
            .conn
            .query_row("SELECT local, source_module FROM import_names WHERE file='App.java'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((local.as_str(), module.as_str()), ("MathUtil", "util"), "import last segment recorded");
    }

    /// L1.4 JavaScript — `import { x as y }` alias binds cross-file exact; `this.foo()` lands kind
    /// `method`; `const x = () =>` arrow is a def; class methods carry their class.
    #[test]
    fn l1_javascript_fixture_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("lib.js"),
            "export function helper(x) { return x + 1; }\n\
             export const arrow = (x) => helper(x);\n\
             export class Widget {\n    go() { this.helperMethod(); return 1; }\n    helperMethod() { return 1; }\n}\n",
        )
        .unwrap();
        fs::write(root.join("app.js"), "import { helper as h } from \"./lib.js\";\nfunction run() { return h(1); }\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "helper", "run"), ("exact".into(), Some("lib.js".into())), "import-as alias binds");
        assert_eq!(edge_kinds(&s, "helperMethod", "go"), ("exact".into(), "method".into()), "this.foo() -> method");
        assert_eq!(parent_of(&s, "go").as_deref(), Some("Widget"), "method def carries class");
        // the arrow fn is a def, and the call inside it is attributed to it
        assert_eq!(resolved(&s, "helper", "arrow").0, "exact", "arrow fn encloses its calls");
    }

    /// L1.4 TypeScript + TSX — two grammars, ONE language: a `.tsx` caller resolves into a `.ts`
    /// def exactly (import-as alias); method call lands kind `method`; methods carry their class.
    #[test]
    fn l1_typescript_tsx_fixture_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("lib.ts"),
            "export function helper(x: number): number { return x + 1; }\n\
             export class Widget {\n    go(): number { this.helperMethod(); return 1; }\n    helperMethod(): number { return 1; }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("app.tsx"),
            "import { helper as h } from \"./lib\";\nexport function App() {\n    return <div>{h(1)}</div>;\n}\n",
        )
        .unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "helper", "App"), ("exact".into(), Some("lib.ts".into())), ".tsx caller -> .ts def (one language)");
        assert_eq!(edge_kinds(&s, "helperMethod", "go"), ("exact".into(), "method".into()), "this.foo() -> method");
        assert_eq!(parent_of(&s, "go").as_deref(), Some("Widget"), "method def carries class");
    }

    /// L1.2 — polyglot language scoping: the same fn name defined in `.py` AND `.rs`; each caller
    /// resolves ONLY to its own language's def — exact on both sides, no cross-language ambiguity.
    #[test]
    fn l1_polyglot_language_scoping() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def shared():\n    return 1\n").unwrap();
        fs::write(root.join("b.py"), "def py_caller():\n    shared()\n").unwrap();
        fs::write(root.join("a.rs"), "pub fn shared() -> i32 { 1 }\n").unwrap();
        fs::write(root.join("b.rs"), "fn rs_caller() { shared(); }\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM symbols WHERE name='shared'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "two same-named defs across languages");
        assert_eq!(resolved(&s, "shared", "py_caller"), ("exact".into(), Some("a.py".into())), "python caller -> python def only");
        assert_eq!(resolved(&s, "shared", "rs_caller"), ("exact".into(), Some("a.rs".into())), "rust caller -> rust def only");
    }

    /// L1.4 — delta==rebuild equivalence on a MIXED-language repo: a Rust-only rename relabels the
    /// Rust edge (and leaves Python/Go untouched), a Go file is deleted, a new Rust def resolves
    /// the dangling call again — and after all of it the delta-maintained graph equals a fresh
    /// cold rebuild.
    #[test]
    fn l1_mixed_language_delta_equals_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("lib.py"), "def target():\n    return 1\n").unwrap();
        fs::write(root.join("app.py"), "def py_use():\n    return target()\n").unwrap();
        fs::write(root.join("lib.rs"), "pub fn target() -> i32 { 1 }\n").unwrap();
        fs::write(root.join("app.rs"), "fn rs_use() { target(); }\n").unwrap();
        fs::write(root.join("lib.go"), "package main\n\nfunc Target() int { return 1 }\n").unwrap();
        fs::write(root.join("app.go"), "package main\n\nfunc GoUse() int { return Target() }\n").unwrap();
        let mut s = Store::open(root).unwrap();
        s.index_repo(root).unwrap();

        assert_eq!(resolved(&s, "target", "py_use").0, "exact");
        assert_eq!(resolved(&s, "target", "rs_use").0, "exact");
        assert_eq!(resolved(&s, "Target", "GoUse").0, "exact");

        // rename the Rust def ONLY -> the Rust edge (unchanged caller file) relabels unresolved;
        // the same-named Python edge is untouched (language scoping in pass-B too)
        fs::write(root.join("lib.rs"), "pub fn target2() -> i32 { 1 }\n").unwrap();
        let st = s.refresh().unwrap();
        assert_eq!(st.changed, 1, "only lib.rs re-parsed");
        assert_eq!(resolved(&s, "target", "rs_use").0, "unresolved", "rust rename relabels the rust edge");
        assert_eq!(resolved(&s, "target", "py_use").0, "exact", "python edge untouched by the rust rename");

        // delete the Go lib -> its edge dangles honestly
        fs::remove_file(root.join("lib.go")).unwrap();
        let st2 = s.refresh().unwrap();
        assert_eq!(st2.deleted, 1);
        assert_eq!(resolved(&s, "Target", "GoUse").0, "unresolved");

        // a new Rust file re-defines target -> the dangling Rust call resolves again
        fs::write(root.join("new.rs"), "pub fn target() -> i32 { 3 }\n").unwrap();
        s.refresh().unwrap();
        assert_eq!(resolved(&s, "target", "rs_use"), ("exact".into(), Some("new.rs".into())));

        // EQUIVALENCE: delta-maintained mixed-language graph == from-scratch rebuild
        let delta_proj = graph_projection(&s);
        let mut s2 = Store::open(root).unwrap();
        s2.index_repo(root).unwrap();
        assert_eq!(delta_proj, graph_projection(&s2), "mixed-language delta == rebuild");
    }
}
