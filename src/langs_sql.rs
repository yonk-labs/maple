//! L2 — SQL-dialect walks: T-SQL (`tree-sitter-sequel-tsql`), PostgreSQL (`tree-sitter-postgres`,
//! outer grammar + its bundled plpgsql grammar for dollar-quoted bodies), Oracle PL/SQL
//! (`tree-sitter-plsql`, pinned git rev). Universal tier only, shallow and honest:
//! - defs: `CREATE PROCEDURE/FUNCTION/TRIGGER` -> `function` (parent = schema/package where the
//!   name is qualified); Oracle packages (spec and body) -> `class` containers; package BODY
//!   members carry `parent_class` = package. Spec/forward DECLARATIONS emit no def (C-header honesty).
//! - calls: bare `proc(x)` / `CALL` / `EXEC` / `PERFORM` -> kind `func`; qualified
//!   `pkg.proc()` / `schema.fn()` -> kind `method` on the member name. One receiver hint exists:
//!   PL/SQL's package qualifier (`pkg.proc()` -> hint `pkg`) — it's syntactically free, like a Go
//!   receiver. Dynamic SQL stays unextracted (honest unresolved-by-absence).
//! - identifiers fold case in every dialect, so every extracted name is lowercased (`norm`); the
//!   store's case-sensitive resolution needs no change.
//! - common builtins (`count`, `nvl`, `getdate`, ...) are skipped at extraction so query-embedded
//!   calls don't flood the graph with unresolved edges — each list is a small documented const,
//!   extend freely.
//! - a file that parses clean but defines no symbols (DDL-only migration) sets `symbolless_ok`,
//!   suppressing the store's "suspect" flag.

use crate::langs::{find_child, tree_for, walk_children};
use crate::parser::{first_line, text, CallSite, Definition, ParsedFile};
use tree_sitter::{Node, Parser, Tree};

/// SQL identifiers are case-insensitive (pg folds down, Oracle/T-SQL fold up) — lowercase every
/// def/call/parent name at extraction. Quoting (`"X"`, `[X]`, backticks) is stripped and quoted
/// mixed-case identifiers fold too.
/// ponytail: quote-preserved case (rare in proc code) would need store-side collation — revisit
/// only if a real corpus needs it.
fn norm(s: &str) -> String {
    s.trim().trim_matches(|c| matches!(c, '"' | '[' | ']' | '`')).to_lowercase()
}

fn mk_def(name: &str, kind: &str, parent: Option<&str>, node: Node, src: &[u8]) -> Definition {
    Definition {
        name: norm(name),
        kind: kind.into(),
        parent_class: parent.map(norm),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        signature: first_line(node, src),
        ret_class: None,
        base_class: None,
        docstring: None,
    }
}

fn push_call(out: &mut ParsedFile, builtins: &[&str], raw: &str, kind: &str, line: usize, enclosing: &str) {
    let name = norm(raw);
    if name.is_empty() || builtins.contains(&name.as_str()) {
        return;
    }
    out.calls.push(CallSite {
        name,
        kind: kind.into(),
        line,
        enclosing: enclosing.to_string(),
        receiver_class: None,
    });
}

/// Calls under a parse-ERROR node are recovery artifacts (real corpora produced "callees" like
/// `rem` — a SQL*Plus directive — and word fragments), so call extraction skips error regions.
/// Defs still extract there: their names come from field-anchored nodes and error-wrapped
/// `CREATE PROCEDURE` headers are usually real (legacy T-SQL quirks land whole bodies in ERROR).
fn entering_error(node: Node, in_error: bool) -> bool {
    in_error || node.is_error()
}

// ---- T-SQL --------------------------------------------------------------------

/// Common T-SQL builtins whose query-embedded calls would flood the graph as unresolved noise.
const TSQL_BUILTINS: &[&str] = &[
    "abs", "avg", "cast", "ceiling", "charindex", "coalesce", "concat", "convert", "count",
    "dateadd", "datediff", "datename", "datepart", "day", "dense_rank", "error_message",
    "error_number", "floor", "format", "getdate", "getutcdate", "iif", "isnull", "isnumeric",
    "lag", "lead", "left", "len", "lower", "ltrim", "max", "min", "month", "newid", "ntile",
    "nullif", "object_id", "patindex", "rank", "replace", "right", "round", "row_number",
    "rtrim", "scope_identity", "stuff", "string_agg", "string_split", "substring", "sum",
    "sysdatetime", "sysutcdatetime", "try_cast", "try_convert", "upper", "year",
];

/// `GO` batch separators live on their own line (sqlcmd rule: optionally `GO <count>`) but the
/// grammar mis-parses them — rewrite each such line to `;` so the grammar sees a plain statement
/// boundary. Line-wise rewrite, so every extracted line number stays exact.
///
/// Procedure-option `WITH` lines (`WITH EXECUTE AS OWNER`, `WITH RECOMPILE`, ...) sit between the
/// proc header and `AS` and the grammar doesn't know them — one collapses the entire definition
/// into an ERROR (every SSDT-generated proc uses `WITH EXECUTE AS ...`). Blank them. CTE
/// disambiguation: a CTE line (`WITH x AS (`) always involves a `(`; a proc-option line never
/// does, and its second word comes from a tiny closed keyword set nobody names a CTE after.
fn strip_go_lines(src: &str) -> String {
    let mut lines: Vec<String> = src
        .lines()
        .map(|line| {
            let t = line.trim();
            let is_go = t.eq_ignore_ascii_case("go")
                || (t.len() > 2
                    && t.is_char_boundary(2)
                    && t[..2].eq_ignore_ascii_case("go")
                    && t[2..].starts_with(char::is_whitespace)
                    && t[2..].trim().chars().all(|c| c.is_ascii_digit()));
            if is_go {
                return ";".to_string();
            }
            if !t.contains('(') {
                let words: Vec<String> =
                    t.split_whitespace().take(3).map(str::to_lowercase).collect();
                let opt = matches!(
                    words.get(1).map(String::as_str),
                    Some("recompile" | "encryption" | "schemabinding" | "native_compilation")
                ) || (words.get(1).map(String::as_str) == Some("execute")
                    && words.get(2).map(String::as_str) == Some("as"));
                if words.first().map(String::as_str) == Some("with") && opt {
                    return String::new();
                }
            }
            line.to_string()
        })
        .collect();
    fix_tsql_proc_shapes(&mut lines);
    lines.join("\n")
}

/// Two classic T-SQL proc shapes the grammar doesn't know, both the house style of entire real
/// codebases (DNN: 500+ procs) and both collapsing the WHOLE definition (defs AND calls) when hit:
///
/// 1. bare parameter lists (`CREATE PROCEDURE dbo.X` newline `@a int, @b int` newline `AS`) ->
///    append `(` to the header line, prefix the closing `AS` line with `) `.
/// 2. bodies without BEGIN/END (`AS <statements>` to end of batch) -> append ` begin` after the
///    AS token and close with `end` at the batch terminator: the `;` line GO became, the next
///    CREATE header, or EOF (one appended line at EOF shifts no existing row).
///
/// Deterministic and line-preserving for all existing content: only columns shift, and rows are
/// all maple records. The param scan aborts (no edit) if a body keyword appears before `AS` —
/// never guess. Single-line bare-param headers (`CREATE PROC p @x int AS ...`) are left alone:
/// rarer, and wrongly splicing one line is worse than missing it.
fn fix_tsql_proc_shapes(lines: &mut Vec<String>) {
    fn first_word(s: &str) -> String {
        s.split_whitespace().next().unwrap_or("").to_lowercase()
    }
    // create [or alter] proc|procedure [name-with-no-params-yet]; `bare` = header line ends at
    // the name (candidate for param wrapping). Functions keep their own body grammar — procs only.
    fn proc_header(line: &str) -> Option<bool> {
        let w: Vec<String> = line.split_whitespace().map(str::to_lowercase).collect();
        let rest: &[String] = match w.split_first() {
            Some((c, rest)) if c == "create" => rest,
            _ => return None,
        };
        let rest = match rest.split_first() {
            Some((o, r)) if o == "or" => match r.split_first() {
                Some((a, r2)) if a == "alter" => r2,
                _ => return None,
            },
            _ => rest,
        };
        match rest.split_first() {
            Some((kw, tail)) if matches!(kw.as_str(), "proc" | "procedure") => {
                Some(tail.len() == 1 && !tail[0].contains('('))
            }
            _ => None,
        }
    }
    let mut i = 0;
    while i < lines.len() {
        let Some(bare) = proc_header(&lines[i]) else {
            i += 1;
            continue;
        };
        // find the standalone AS that opens the body (skipping the param block when present)
        let mut as_line = None;
        let mut j = i + 1;
        while j < lines.len() {
            let fw = first_word(&lines[j]);
            if fw == "as" || fw == ")" && lines[j].trim_start()[1..].trim_start().to_lowercase().starts_with("as") {
                as_line = Some(j);
                break;
            }
            if matches!(fw.as_str(), "begin" | "select" | "insert" | "update" | "delete" | "declare" | "set" | "create" | "exec" | "execute" | "return") {
                break;
            }
            j += 1;
        }
        let Some(k) = as_line else {
            i += 1;
            continue;
        };
        // shape 1: wrap a bare param block (first non-blank/comment line after header starts @)
        if bare {
            let mut p = i + 1;
            while p < lines.len() && (lines[p].trim().is_empty() || lines[p].trim_start().starts_with("--")) {
                p += 1;
            }
            if p < k && lines[p].trim_start().starts_with('@') {
                lines[i].push('(');
                let indent = lines[k].len() - lines[k].trim_start().len();
                lines[k].insert_str(indent, ") ");
            }
        }
        // shape 2: body without BEGIN — inject `begin` after AS, `end` at the batch terminator
        let after_as = {
            let t = lines[k].trim_start();
            let t = t.strip_prefix(") ").unwrap_or(t);
            t[2..].trim_start().to_string() // past "as"/"AS"
        };
        let next_content = if after_as.is_empty() {
            (k + 1..lines.len())
                .map(|n| lines[n].trim())
                .find(|t| !t.is_empty() && !t.starts_with("--"))
                .unwrap_or("")
                .to_string()
        } else {
            after_as
        };
        if !next_content.to_lowercase().starts_with("begin") {
            let mut end_at = None; // line index of `;` (from GO) or next CREATE, else EOF
            for (n, line) in lines.iter().enumerate().skip(k + 1) {
                let t = line.trim();
                if t == ";" || first_word(t) == "create" {
                    end_at = Some(n);
                    break;
                }
            }
            // insert right after the `as` token (there may be body content on the same line)
            let mut pos = lines[k].len() - lines[k].trim_start().len();
            if lines[k][pos..].starts_with(") ") {
                pos += 2;
            }
            pos += 2; // the as/AS token itself
            lines[k].insert_str(pos, " begin");
            match end_at {
                Some(n) => {
                    let indent = lines[n].len() - lines[n].trim_start().len();
                    lines[n].insert_str(indent, "end ");
                    i = n;
                }
                None => {
                    lines.push("end".to_string());
                    i = lines.len();
                }
            }
        } else {
            i = k;
        }
        i += 1;
    }
}

pub fn parse_tsql(src: &str) -> anyhow::Result<ParsedFile> {
    let pre = strip_go_lines(src);
    let tree = tree_for(&pre, tree_sitter_sequel_tsql::LANGUAGE.into(), "tsql")?;
    let mut out = ParsedFile::default();
    walk_tsql(tree.root_node(), pre.as_bytes(), &mut out, "<module>", false);
    out.symbolless_ok = !tree.root_node().has_error();
    Ok(out)
}

/// (schema, name) from a tsql `object_reference` (fields: database/schema/name).
fn obj_ref_parts<'a>(node: Node, src: &'a [u8]) -> (Option<&'a str>, Option<&'a str>) {
    (
        node.child_by_field_name("schema").map(|n| text(n, src)),
        node.child_by_field_name("name").map(|n| text(n, src)),
    )
}

fn walk_tsql(node: Node, src: &[u8], out: &mut ParsedFile, enclosing: &str, in_error: bool) {
    let in_error = entering_error(node, in_error);
    let mut def_name: Option<String> = None;
    match node.kind() {
        "create_procedure" | "create_function" | "alter_procedure" | "alter_function" => {
            if let Some(or) = find_child(node, "object_reference") {
                let (schema, name) = obj_ref_parts(or, src);
                if let Some(nm) = name {
                    let d = mk_def(nm, "function", schema, node, src);
                    def_name = Some(d.name.clone());
                    out.defs.push(d);
                }
            }
        }
        // `CREATE TRIGGER name ON table ...` — the FIRST object_reference is the trigger's name.
        "create_trigger" => {
            if let Some(or) = find_child(node, "object_reference") {
                let (schema, name) = obj_ref_parts(or, src);
                if let Some(nm) = name {
                    let d = mk_def(nm, "function", schema, node, src);
                    def_name = Some(d.name.clone());
                    out.defs.push(d);
                }
            }
        }
        // `foo(...)` in any expression, and `EXEC [dbo.]proc` — schema-qualified -> method kind.
        "invocation" | "execute_statement" if !in_error => {
            if let Some(or) = find_child(node, "object_reference") {
                let (schema, name) = obj_ref_parts(or, src);
                if let Some(nm) = name {
                    let kind = if schema.is_some() { "method" } else { "func" };
                    push_call(out, TSQL_BUILTINS, nm, kind, node.start_position().row + 1, enclosing);
                }
            }
        }
        _ => {}
    }
    let enc = def_name.as_deref().unwrap_or(enclosing);
    walk_children(node, |c| walk_tsql(c, src, out, enc, in_error));
}

// ---- Oracle PL/SQL --------------------------------------------------------------

/// Common Oracle builtins — same rationale as `TSQL_BUILTINS`. (`sysdate` is a bare identifier,
/// not a call, so it never reaches extraction.)
const PLSQL_BUILTINS: &[&str] = &[
    "abs", "add_months", "ascii", "avg", "cast", "ceil", "chr", "coalesce", "count", "decode",
    "dense_rank", "extract", "floor", "greatest", "initcap", "instr", "lag", "last_day", "lead",
    "least", "length", "listagg", "lower", "lpad", "ltrim", "max", "min", "mod",
    "bfilename", "empty_blob", "empty_clob", "hextoraw", "numtodsinterval", "numtoyminterval",
    "ora_hash", "rawtohex", "sys_connect_by_path", "to_dsinterval", "to_timestamp",
    "to_yminterval", "unistr", "xmltype",
    // Oracle-shipped object-type constructors (spatial) — ubiquitous in data dumps
    "sdo_elem_info_array", "sdo_geometry", "sdo_ordinate_array", "sdo_point_type",
    "months_between", "next_day", "nullif", "nvl", "nvl2", "power", "rank", "regexp_instr",
    "regexp_like", "regexp_replace", "regexp_substr", "replace", "round", "row_number", "rpad",
    "rtrim", "sign", "sqrt", "substr", "sum", "to_char", "to_date", "to_number", "trim", "trunc",
    "upper",
];

/// SQL*Plus client directives are line-oriented and not PL/SQL — the grammar doesn't know them,
/// and a run of `rem` prose lines produces an ERROR region big enough to swallow the following
/// `CREATE PROCEDURE` whole (seen in Oracle's own sample schemas). Blank those lines
/// (line-preserving, like the T-SQL `GO` rewrite). `set` needs care: `UPDATE ...\nSET col = 1`
/// puts SET at line start in real SQL, so it's blanked only for a known SQL*Plus parameter name
/// with no `=` after it.
///
/// `grant`/`revoke` are blanked for a different reason: the grammar has no GRANT rule and
/// tree-sitter's error recovery goes QUADRATIC on long grant runs — a real production DDL dump
/// with ~150k grant lines went from a projected ~34 min to seconds. They carry no graph
/// information (no defs, no calls). A grant continuation line or a `GRANT` line inside a dynamic
/// SQL string can survive blanking or lose a line of literal text — both inert for extraction.
fn strip_sqlplus_lines(src: &str) -> String {
    const DIRECTIVES: &[&str] =
        &["rem", "remark", "prompt", "spool", "show", "whenever", "define", "undefine", "grant", "revoke"];
    const SET_PARAMS: &[&str] = &[
        "autocommit", "colsep", "echo", "feedback", "heading", "linesize", "long",
        "longchunksize", "newpage", "numwidth", "pagesize", "pause", "serveroutput",
        "sqlblanklines", "tab", "termout", "timing", "trimout", "trimspool", "verify", "wrap",
    ];
    // EDB DDL-extractor dumps embed object-dependency CSV between explicit comment markers —
    // ~20k+ rows of quoted tuples that aren't SQL and send error recovery quadratic (measured:
    // ~275s of a 280s parse on a real 660k-line production dump). The markers are exact, so
    // blanking the enclosed section is deterministic. CSV blobs WITHOUT these markers stay slow —
    // ponytail: add shape-based CSV detection only if a non-EDB corpus ever needs it.
    let mut in_deps = false;
    src.lines()
        .map(|line| {
            let t = line.trim();
            if t == "--START_OF_DEPENDENCIES" {
                in_deps = true;
            } else if t == "--END_OF_DEPENDENCIES" {
                in_deps = false;
            }
            if in_deps {
                return "";
            }
            let t = line.trim_start();
            let mut words = t.split_whitespace();
            let first = words.next().unwrap_or("").to_lowercase();
            let second = words.next().unwrap_or("").to_lowercase();
            let third = words.next().unwrap_or("");
            // '#' never starts a SQL/PLSQL statement — extractor tools (EDB DDL extractor) emit
            // `####...` banner lines whose ERROR recovery swallows whole neighboring statements.
            let is_directive = t.starts_with('@')
                || t.starts_with('#')
                || DIRECTIVES.contains(&first.as_str())
                || (first == "set" && SET_PARAMS.contains(&second.as_str()) && !third.starts_with('='));
            if is_directive {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_plsql(src: &str) -> anyhow::Result<ParsedFile> {
    let pre = strip_sqlplus_lines(src);
    let tree = tree_for(&pre, tree_sitter_plsql::language(), "plsql")?;
    let mut out = ParsedFile::default();
    walk_plsql(tree.root_node(), pre.as_bytes(), &mut out, "<module>", None, false);
    out.symbolless_ok = !tree.root_node().has_error();
    Ok(out)
}

fn walk_plsql(
    node: Node,
    src: &[u8],
    out: &mut ParsedFile,
    enclosing: &str,
    package: Option<&str>,
    in_error: bool,
) {
    let in_error = entering_error(node, in_error);
    let mut def_name: Option<String> = None;
    let mut child_package = package;
    match node.kind() {
        // package spec AND body are both real `class`-container definitions; their members differ
        // below (spec members are declarations -> no defs; body members are definitions -> defs).
        "create_package" | "create_package_body" => {
            if let Some(nm) = node.child_by_field_name("package_name") {
                let d = mk_def(text(nm, src), "class", None, node, src);
                def_name = Some(d.name.clone());
                out.defs.push(d);
            }
        }
        // object types are class containers too — `cust_address_typ(...)` ctor calls then resolve
        // to the type. Only the SPEC emits the class def (attributes live there); the BODY just
        // implements members, and a second same-name def would turn every ctor call ambiguous.
        // (Packages differ: spec AND body both emit — nothing ever "calls" a package name, so the
        // duplicate is harmless there and both containers are real.)
        "create_type" => {
            if let Some(nm) = find_child(node, "plsql_type_source").and_then(|ts| find_child(ts, "identifier")) {
                let d = mk_def(text(nm, src), "class", None, node, src);
                def_name = Some(d.name.clone());
                out.defs.push(d);
            }
        }
        // container only — member definitions inside carry the type as parent, no class def
        "create_type_body" => {
            if let Some(nm) = node.child_by_field_name("type_name") {
                def_name = Some(norm(text(nm, src)));
            }
        }
        // standalone `CREATE [OR REPLACE] PROCEDURE/FUNCTION [schema.]name`
        "create_procedure" | "create_function" => {
            let field = if node.kind() == "create_procedure" { "prc_name" } else { "fnc_name" };
            if let Some(nm) = node.child_by_field_name(field) {
                let schema = node.child_by_field_name("schema_name").map(|s| text(s, src));
                let d = mk_def(text(nm, src), "function", schema, node, src);
                def_name = Some(d.name.clone());
                out.defs.push(d);
            }
        }
        // members inside a package BODY (or nested in another definition — then parent is None,
        // same rule as nested fns in every other language)
        "procedure_definition" | "function_definition" => {
            let field = if node.kind() == "procedure_definition" { "prc_name" } else { "fnc_name" };
            if let Some(nm) = node.child_by_field_name(field) {
                let d = mk_def(text(nm, src), "function", package, node, src);
                def_name = Some(d.name.clone());
                out.defs.push(d);
            }
            child_package = None;
        }
        "create_trigger" => {
            if let Some(nm) = node.child_by_field_name("trigger_name") {
                let schema = node.child_by_field_name("schema_name").map(|s| text(s, src));
                let d = mk_def(text(nm, src), "function", schema, node, src);
                def_name = Some(d.name.clone());
                out.defs.push(d);
            }
        }
        // `name(...)` anywhere — qualified (`pkg.proc`, `schema.pkg.proc`) -> method kind on the
        // member name, with the qualifier as a receiver-class hint: it's syntactically free (like
        // a Go receiver) and lets `htp.print()` vs `htf.print()` resolve to the right package's
        // member instead of cross-package ambiguity. Validation happens at resolution time (the
        // hint only applies when it names class-kind symbols); a schema qualifier names no class
        // and falls back to today's behavior. PL/SQL can't syntactically split collection indexing
        // from calls, so `arr(i)` is extracted too — honest over-report, resolves unresolved.
        "ref_call" if !in_error => {
            if let Some(re) = find_child(node, "referenced_element") {
                if let Some(nm) = re.child_by_field_name("ref_name") {
                    let parent = re.child_by_field_name("ref_name_parent").map(|p| norm(text(p, src)));
                    let kind = if parent.is_some() || re.child_by_field_name("schema_name").is_some() {
                        "method"
                    } else {
                        "func"
                    };
                    let line = node.start_position().row + 1;
                    let name = norm(text(nm, src));
                    if !name.is_empty() && !PLSQL_BUILTINS.contains(&name.as_str()) {
                        out.calls.push(CallSite {
                            name,
                            kind: kind.into(),
                            line,
                            enclosing: enclosing.to_string(),
                            receiver_class: parent,
                        });
                    }
                }
            }
        }
        _ => {}
    }
    let enc = def_name.as_deref().unwrap_or(enclosing);
    let container = matches!(
        node.kind(),
        "create_package" | "create_package_body" | "create_type" | "create_type_body"
    );
    let pkg = if def_name.is_some() && container { def_name.as_deref() } else { child_package };
    walk_children(node, |c| walk_plsql(c, src, out, enc, pkg, in_error));
}

// ---- PostgreSQL -----------------------------------------------------------------

/// Common pg builtins — same rationale as `TSQL_BUILTINS`.
const PG_BUILTINS: &[&str] = &[
    "abs", "age", "array_agg", "avg", "ceil", "ceiling", "char_length", "clock_timestamp",
    "coalesce", "concat", "concat_ws", "count", "currval", "date_part", "date_trunc",
    "dense_rank", "extract", "first_value", "floor", "format", "gen_random_uuid",
    "generate_series", "greatest", "jsonb_agg", "jsonb_build_object", "json_agg",
    "json_build_object", "lag", "last_value", "lastval", "lead", "least", "left", "length",
    "lower", "lpad", "ltrim", "max", "md5", "min", "mod", "nextval", "now", "nullif", "position",
    "power", "random", "rank", "regexp_match", "regexp_matches", "regexp_replace", "replace",
    "right", "round", "row_number", "rpad", "rtrim", "setval", "split_part", "sqrt",
    "statement_timestamp", "string_agg", "substr", "substring", "sum", "to_char", "to_date",
    "to_number", "to_timestamp", "trim", "unnest", "upper",
];

pub fn parse_postgres(src: &str) -> anyhow::Result<ParsedFile> {
    let tree = tree_for(src, tree_sitter_postgres::LANGUAGE.into(), "postgres")?;
    let mut out = ParsedFile::default();
    let mut ps = PgParsers::new()?;
    walk_pg(tree.root_node(), src.as_bytes(), &mut out, "<module>", &mut ps, false);
    out.symbolless_ok = !tree.root_node().has_error();
    Ok(out)
}

/// One parser per grammar per file, reused across every body/fragment re-parse.
struct PgParsers {
    pl: Parser,
    sql: Parser,
}

impl PgParsers {
    fn new() -> anyhow::Result<Self> {
        let mut pl = Parser::new();
        pl.set_language(&tree_sitter_postgres::LANGUAGE_PLPGSQL.into())
            .map_err(|e| anyhow::anyhow!("load plpgsql grammar: {e}"))?;
        let mut sql = Parser::new();
        sql.set_language(&tree_sitter_postgres::LANGUAGE.into())
            .map_err(|e| anyhow::anyhow!("load postgres grammar: {e}"))?;
        Ok(Self { pl, sql })
    }
}

/// `analytics.compute` -> ["analytics", "compute"] — the grammar's func_name text, split on dots.
/// `norm` (in the caller) strips quoting per part.
fn dotted_parts(s: &str) -> Vec<&str> {
    s.split('.').map(str::trim).filter(|p| !p.is_empty()).collect()
}

fn walk_pg(node: Node, src: &[u8], out: &mut ParsedFile, enclosing: &str, ps: &mut PgParsers, in_error: bool) {
    let in_error = entering_error(node, in_error);
    let mut def_name: Option<String> = None;
    match node.kind() {
        // covers CREATE FUNCTION and CREATE PROCEDURE (one grammar rule for both)
        "CreateFunctionStmt" => {
            if let Some(fname) = find_descendant(node, "func_name") {
                let full = text(fname, src);
                let parts = dotted_parts(full);
                if let Some(nm) = parts.last() {
                    let schema = (parts.len() > 1).then(|| parts[parts.len() - 2]);
                    let d = mk_def(nm, "function", schema, node, src);
                    def_name = Some(d.name.clone());
                    out.defs.push(d);
                }
            }
            // body re-parse extracts CALLS — same error-region rule as `func_application`
            if let (Some(enc), false) = (def_name.as_deref(), in_error) {
                pg_function_body(node, src, out, enc, ps);
            }
        }
        // `DO $$ ... $$` — anonymous plpgsql block, calls attributed to <module>
        "DoStmt" if !in_error => {
            if let Some(body) = find_descendant(node, "dollar_quoted_string") {
                pg_plpgsql_body(body, src, out, enclosing, ps);
            }
        }
        // plain SQL calls in the outer file (SELECT setup(); triggers' EXECUTE FUNCTION; defaults)
        "func_application" if !in_error => {
            if let Some(fname) = find_child(node, "func_name") {
                pg_push_func(out, text(fname, src), node.start_position().row + 1, enclosing);
            }
        }
        _ => {}
    }
    let enc = def_name.as_deref().unwrap_or(enclosing);
    walk_children(node, |c| walk_pg(c, src, out, enc, ps, in_error));
}

fn pg_push_func(out: &mut ParsedFile, full: &str, line: usize, enclosing: &str) {
    let parts = dotted_parts(full);
    if let Some(nm) = parts.last() {
        let kind = if parts.len() > 1 { "method" } else { "func" };
        push_call(out, PG_BUILTINS, nm, kind, line, enclosing);
    }
}

/// first descendant of `kind` (breadth-limited manual DFS; grammar guarantees shallow placement)
fn find_descendant<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == kind {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
    }
    None
}

/// A CreateFunctionStmt's body is a string literal — find `LANGUAGE x` and the dollar-quoted body,
/// then re-parse: plpgsql via the bundled plpgsql grammar, `LANGUAGE sql` directly as statements.
/// Single-quoted bodies stay defs-only (escape soup; dollar quoting is the norm).
fn pg_function_body(node: Node, src: &[u8], out: &mut ParsedFile, enclosing: &str, ps: &mut PgParsers) {
    let mut language: Option<String> = None;
    let mut body: Option<Node> = None;
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "createfunc_opt_item" {
            if find_child(n, "kw_language").is_some() {
                if let Some(v) = n.named_child(n.named_child_count().saturating_sub(1)) {
                    // `LANGUAGE plpgsql` and the quoted form `LANGUAGE 'plpgsql'` both count
                    language = Some(text(v, src).trim_matches('\'').to_lowercase());
                }
            } else if let Some(b) = find_descendant(n, "dollar_quoted_string") {
                body = Some(b);
            }
            continue;
        }
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
    }
    match (language.as_deref(), body) {
        (Some("plpgsql"), Some(b)) => pg_plpgsql_body(b, src, out, enclosing, ps),
        (Some("sql"), Some(b)) => {
            if let Some(blanked) = blank_dollar_delims(text(b, src)) {
                pg_sql_fragment(&blanked, b.start_position().row, out, enclosing, ps);
            }
        }
        _ => {} // no body string (BEGIN ATOMIC parses inline via the generic walk), C functions, etc.
    }
}

/// Replace the `$tag$` delimiters with equal-length spaces so the inner text keeps its exact rows
/// (tags never contain newlines), then hand back the row-preserving body.
fn blank_dollar_delims(raw: &str) -> Option<String> {
    if !raw.starts_with('$') {
        return None;
    }
    let close = raw[1..].find('$')? + 1;
    let tag_len = close + 1;
    if raw.len() < tag_len * 2 || !raw.ends_with(&raw[..tag_len]) {
        return None;
    }
    let mut s = String::with_capacity(raw.len());
    s.push_str(&" ".repeat(tag_len));
    s.push_str(&raw[tag_len..raw.len() - tag_len]);
    s.push_str(&" ".repeat(tag_len));
    Some(s)
}

/// Parse a dollar-quoted plpgsql body with the bundled plpgsql grammar (row-preserving blank of the
/// delimiters, so `body_row + inner_row` is the absolute row). The plpgsql grammar keeps embedded
/// SQL as opaque `sql_expression` leaves — each one re-parses through `pg_sql_fragment`.
fn pg_plpgsql_body(body: Node, src: &[u8], out: &mut ParsedFile, enclosing: &str, ps: &mut PgParsers) {
    let Some(blanked) = blank_dollar_delims(text(body, src)) else { return };
    let Some(tree) = ps.pl.parse(&blanked, None) else { return };
    let base = body.start_position().row;
    // the re-parsed plpgsql tree gets the same error-region call suppression as the outer walks:
    // a body with a structural error (unclosed LOOP/IF) recovers statements INSIDE the ERROR node,
    // and their membership is not established — extracting them would be a guess.
    let mut stack = vec![(tree.root_node(), false)];
    while let Some((n, err)) = stack.pop() {
        let err = entering_error(n, err);
        if n.kind() == "sql_expression" {
            if !err {
                pg_sql_fragment(text(n, blanked.as_bytes()), base + n.start_position().row, out, enclosing, ps);
            }
            continue;
        }
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push((child, err));
        }
    }
}

/// Extract calls from one SQL fragment: parse as statement text first (covers plpgsql
/// `stmt_execsql` bodies like `SELECT ... INTO ...`), else wrapped as `SELECT <expr>;` (covers
/// PERFORM/CALL/IF/assignment expressions — the prefix adds no rows, and rows are all we record).
/// Fragments that parse neither way are skipped: no call-site is invented (never guess).
fn pg_sql_fragment(fragment: &str, base_row: usize, out: &mut ParsedFile, enclosing: &str, ps: &mut PgParsers) {
    if fragment.trim().is_empty() {
        return;
    }
    let direct = ps.sql.parse(fragment, None).filter(|t| !t.root_node().has_error());
    let (tree, text_owned): (Tree, String) = match direct {
        Some(t) => (t, fragment.to_string()),
        None => {
            let wrapped = format!("SELECT {fragment};");
            match ps.sql.parse(&wrapped, None).filter(|t| !t.root_node().has_error()) {
                Some(t) => (t, wrapped),
                None => return,
            }
        }
    };
    let src = text_owned.as_bytes();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "func_application" {
            if let Some(fname) = find_child(n, "func_name") {
                pg_push_func(out, text(fname, src), base_row + n.start_position().row + 1, enclosing);
            }
        }
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- T-SQL ----

    #[test]
    fn tsql_defs_calls_and_go() {
        let src = "\
CREATE PROCEDURE dbo.UpdateStats
AS
BEGIN
    SELECT COUNT(1), SUM(Amount) FROM Orders;
    EXEC dbo.LogAccess 1;
    EXEC UpdateHelper;
    SELECT dbo.OrderTotal(2);
END
GO
CREATE FUNCTION dbo.OrderTotal (@OrderId INT) RETURNS INT
AS
BEGIN
    RETURN 1
END
GO 2
";
        let p = parse_tsql(src).unwrap();
        // defs: lowercased, schema as parent, correct lines
        let d = p.defs.iter().find(|d| d.name == "updatestats").unwrap();
        assert_eq!(d.parent_class.as_deref(), Some("dbo"));
        assert_eq!(d.start_line, 1);
        assert!(p.defs.iter().any(|d| d.name == "ordertotal" && d.start_line == 10));
        // qualified EXEC -> method; bare EXEC -> func; invocation in SELECT -> method (dbo-qualified)
        assert!(p.calls.iter().any(|c| c.name == "logaccess" && c.kind == "method" && c.enclosing == "updatestats" && c.line == 5));
        assert!(p.calls.iter().any(|c| c.name == "updatehelper" && c.kind == "func"));
        assert!(p.calls.iter().any(|c| c.name == "ordertotal" && c.kind == "method" && c.line == 7));
        // builtins skipped
        assert!(!p.calls.iter().any(|c| c.name == "count" || c.name == "sum"));
        assert!(p.symbolless_ok, "GO-stripped source parses clean");
    }

    /// SSDT-style `WITH EXECUTE AS OWNER` between header and AS must not collapse the def;
    /// a real CTE (`WITH x AS (...)`) survives the blanking untouched.
    #[test]
    fn tsql_proc_options_blanked_ctes_kept() {
        let src = "\
CREATE PROCEDURE [WebApi].[DeletePackageType](@PackageTypeID int)
WITH EXECUTE AS OWNER
AS BEGIN
    DELETE Warehouse.PackageTypes
    WHERE PackageTypeID = @PackageTypeID;
END
GO
CREATE PROCEDURE Reporting.Summarize
AS BEGIN
    WITH recent AS (SELECT OrderID FROM Orders)
    SELECT dbo.OrderTotal(OrderID) FROM recent;
END
GO
";
        let p = parse_tsql(src).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "deletepackagetype" && d.parent_class.as_deref() == Some("webapi")));
        assert!(p.defs.iter().any(|d| d.name == "summarize"));
        // the CTE body still parses: the call inside it extracts
        assert!(p.calls.iter().any(|c| c.name == "ordertotal" && c.enclosing == "summarize"));
    }

    /// DNN-style procs: bare unparenthesized parameter list AND a body with no BEGIN/END, batch
    /// ending at EOF — both rewritten line-preservingly; def lines must match the original file.
    #[test]
    fn tsql_bare_params_and_no_begin_body() {
        let src = "\
create procedure dbo.AddAnnouncement

@ModuleId       int,
@UserName       nvarchar(100),
@ViewOrder\tint

as

insert into Announcements (ModuleId, CreatedByUser)
values (@ModuleId, @UserName)

select SCOPE_IDENTITY()";
        let p = parse_tsql(src).unwrap();
        let d = p.defs.iter().find(|d| d.name == "addannouncement").expect("def extracted");
        assert_eq!(d.parent_class.as_deref(), Some("dbo"));
        assert_eq!(d.start_line, 1);
        assert!(!p.calls.iter().any(|c| c.name == "scope_identity"), "builtin skipped");
    }

    /// Same shapes across GO-separated batches: the injected END lands on the batch boundary and
    /// the second proc still extracts; calls inside no-BEGIN bodies extract too.
    #[test]
    fn tsql_no_begin_bodies_across_batches() {
        let src = "\
create procedure dbo.First
@x int
as
exec dbo.LogAccess @x
GO
create procedure dbo.Second
as
select 1
GO
";
        let p = parse_tsql(src).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "first" && d.start_line == 1));
        assert!(p.defs.iter().any(|d| d.name == "second" && d.start_line == 6));
        assert!(p.calls.iter().any(|c| c.name == "logaccess" && c.enclosing == "first" && c.line == 4));
    }

    #[test]
    fn tsql_ddl_only_is_symbolless_ok() {
        let p = parse_tsql("CREATE TABLE t (id INT PRIMARY KEY);\n").unwrap();
        assert!(p.defs.is_empty() && p.calls.is_empty());
        assert!(p.symbolless_ok);
    }

    // ---- PL/SQL ----

    #[test]
    fn plsql_package_spec_body_and_standalone() {
        let src = "\
CREATE OR REPLACE PACKAGE order_pkg AS
  PROCEDURE process_order(p_id IN NUMBER);
END order_pkg;
/

CREATE OR REPLACE PACKAGE BODY order_pkg AS
  PROCEDURE process_order(p_id IN NUMBER) IS
    v_total NUMBER;
  BEGIN
    v_total := ORDER_TOTAL(p_id);
    log_pkg.write('processed');
    audit_order(p_id, v_total);
  END process_order;
END order_pkg;
/

CREATE OR REPLACE PROCEDURE audit_order(p_id NUMBER, p_total NUMBER) IS
BEGIN
  INSERT INTO audit_log VALUES (p_id, p_total, SYSDATE);
END;
/
";
        let p = parse_plsql(src).unwrap();
        // package spec + body are class containers; spec's PROCEDURE decl emits NO def
        assert_eq!(p.defs.iter().filter(|d| d.name == "order_pkg" && d.kind == "class").count(), 2);
        let members: Vec<_> = p.defs.iter().filter(|d| d.name == "process_order").collect();
        assert_eq!(members.len(), 1, "spec decl is not a def; only the body definition is");
        assert_eq!(members[0].parent_class.as_deref(), Some("order_pkg"));
        // standalone proc
        assert!(p.defs.iter().any(|d| d.name == "audit_order" && d.parent_class.is_none()));
        // calls: case-folded bare call -> func; pkg-qualified -> method
        assert!(p.calls.iter().any(|c| c.name == "order_total" && c.kind == "func" && c.enclosing == "process_order"));
        assert!(p.calls.iter().any(|c| c.name == "write" && c.kind == "method" && c.enclosing == "process_order"));
        assert!(p.calls.iter().any(|c| c.name == "audit_order" && c.kind == "func"));
        assert!(p.symbolless_ok);
    }

    /// The package qualifier is a receiver hint: `a_pkg.write()` and `b_pkg.write()` each carry
    /// their package, so twin-API packages (Oracle's htp/htf shape) resolve to the right member
    /// instead of cross-package ambiguity.
    #[test]
    fn plsql_package_qualifier_is_receiver_hint() {
        let src = "\
CREATE OR REPLACE PACKAGE BODY a_pkg AS
  PROCEDURE write(s VARCHAR2) IS BEGIN NULL; END;
END a_pkg;
/
CREATE OR REPLACE PACKAGE BODY b_pkg AS
  PROCEDURE write(s VARCHAR2) IS BEGIN NULL; END;
END b_pkg;
/
CREATE OR REPLACE PROCEDURE caller IS
BEGIN
  a_pkg.write('x');
END;
/
";
        let p = parse_plsql(src).unwrap();
        let call = p.calls.iter().find(|c| c.name == "write").unwrap();
        assert_eq!(call.kind, "method");
        assert_eq!(call.receiver_class.as_deref(), Some("a_pkg"));
    }

    /// SQL*Plus directive runs must not swallow the following CREATE (Oracle sample-schema shape);
    /// real `UPDATE ... SET` lines survive the `set` blanking.
    #[test]
    fn plsql_sqlplus_directives_stripped() {
        let src = "\
SET ECHO OFF
SET NUMWIDTH 10
REM **************************************
REM procedure to allow dmls during business hours
REM another prose line that goes on and on
CREATE OR REPLACE PROCEDURE secure_dml
IS
BEGIN
  UPDATE employees
  SET salary = 1
  WHERE id = 5;
END secure_dml;
/
";
        let p = parse_plsql(src).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "secure_dml" && d.start_line == 6), "{:?}", p.defs);
        // the UPDATE's SET line survived blanking (statement intact -> no parse error)
        assert!(p.symbolless_ok, "directives blanked, rest parses clean");
    }

    // ---- PostgreSQL ----

    #[test]
    fn pg_plpgsql_body_calls_with_correct_lines() {
        let src = "\
CREATE OR REPLACE FUNCTION order_total(oid int) RETURNS numeric AS $$
DECLARE
  t numeric;
BEGIN
  SELECT sum(amount) INTO t FROM order_lines WHERE order_id = oid;
  PERFORM log_access(oid);
  IF t IS NULL THEN
    t := default_total();
  END IF;
  CALL analytics.rebuild(oid);
  RETURN t;
END;
$$ LANGUAGE plpgsql;

CREATE FUNCTION plain_sql(x int) RETURNS int AS $$ SELECT order_total(x) $$ LANGUAGE sql;
";
        let p = parse_postgres(src).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "order_total" && d.kind == "function" && d.start_line == 1));
        assert!(p.defs.iter().any(|d| d.name == "plain_sql" && d.start_line == 15));
        // body calls with absolute lines: PERFORM on 6, assignment RHS on 8, qualified CALL on 10
        assert!(p.calls.iter().any(|c| c.name == "log_access" && c.kind == "func" && c.enclosing == "order_total" && c.line == 6));
        assert!(p.calls.iter().any(|c| c.name == "default_total" && c.line == 8));
        assert!(p.calls.iter().any(|c| c.name == "rebuild" && c.kind == "method" && c.line == 10));
        // LANGUAGE sql body parses directly
        assert!(p.calls.iter().any(|c| c.name == "order_total" && c.enclosing == "plain_sql" && c.line == 15));
        // builtins skipped
        assert!(!p.calls.iter().any(|c| c.name == "sum"));
        assert!(p.symbolless_ok);
    }

    #[test]
    fn pg_schema_qualified_def_and_module_call() {
        let src = "\
CREATE FUNCTION analytics.rollup(day date) RETURNS void AS $$
BEGIN
  PERFORM analytics.compute(day);
END;
$$ LANGUAGE plpgsql;

SELECT setup_all();
";
        let p = parse_postgres(src).unwrap();
        let d = p.defs.iter().find(|d| d.name == "rollup").unwrap();
        assert_eq!(d.parent_class.as_deref(), Some("analytics"));
        assert!(p.calls.iter().any(|c| c.name == "compute" && c.kind == "method" && c.enclosing == "rollup"));
        assert!(p.calls.iter().any(|c| c.name == "setup_all" && c.enclosing == "<module>" && c.line == 7));
    }

    /// Review fix — a plpgsql body with a structural error (unclosed IF) recovers trailing
    /// statements INSIDE an ERROR node; their calls must not extract (same rule as the outer
    /// walks). The def itself still extracts.
    #[test]
    fn pg_broken_body_suppresses_error_region_calls() {
        let src = "\
CREATE FUNCTION broken() RETURNS void AS $$
BEGIN
  IF x > 0 THEN
    real_call(x);
  another_call(y);
END;
$$ LANGUAGE plpgsql;
";
        let p = parse_postgres(src).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "broken"), "def still extracts");
        assert!(
            !p.calls.iter().any(|c| c.name == "another_call"),
            "no calls from the ERROR recovery region: {:?}",
            p.calls
        );
    }

    /// Review nit — the quoted `LANGUAGE 'plpgsql'` form counts as plpgsql too.
    #[test]
    fn pg_quoted_language_form() {
        let src = "\
CREATE FUNCTION q() RETURNS void AS $$
BEGIN
  PERFORM quoted_lang_call();
END;
$$ LANGUAGE 'plpgsql';
";
        let p = parse_postgres(src).unwrap();
        assert!(p.calls.iter().any(|c| c.name == "quoted_lang_call" && c.enclosing == "q"));
    }

    #[test]
    fn pg_ddl_only_is_symbolless_ok() {
        let p = parse_postgres("CREATE TABLE t (id int primary key);\n").unwrap();
        assert!(p.defs.is_empty() && p.calls.is_empty());
        assert!(p.symbolless_ok);
    }

    #[test]
    fn blank_dollar_delims_preserves_rows() {
        let body = "$fn$\nBEGIN\nEND;\n$fn$";
        let blanked = blank_dollar_delims(body).unwrap();
        assert_eq!(blanked.len(), body.len());
        assert_eq!(blanked.lines().count(), body.lines().count());
        assert!(blanked.starts_with("    \n"));
        assert!(blank_dollar_delims("'not dollar'").is_none());
    }
}
