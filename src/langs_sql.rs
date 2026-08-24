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
use std::collections::HashMap;
use tree_sitter::{Node, Parser, Tree};

// ---- L3.3: shared DML column-reference helpers (all three dialects) -----------

/// Every `kind`-matching descendant, EXCEPT inside a nested statement of a kind in `stop_at` — a
/// subquery's column references belong to ITS OWN scope, not the enclosing statement's; that
/// nested statement gets its own correctly-scoped extraction when the top-level walk's normal
/// recursion reaches it. Without this boundary, `SELECT a FROM t WHERE x IN (SELECT b FROM u)`
/// would misattribute `b` to `t`'s scope instead of `u`'s.
fn find_descendants_scoped<'t>(node: Node<'t>, kind: &str, stop_at: &[&str]) -> Vec<Node<'t>> {
    fn walk<'t>(node: Node<'t>, kind: &str, stop_at: &[&str], out: &mut Vec<Node<'t>>) {
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            if child.kind() == kind {
                out.push(child); // a column-ref node never nests another one — no need to recurse in
                continue;
            }
            if stop_at.contains(&child.kind()) {
                continue;
            }
            walk(child, kind, stop_at, out);
        }
    }
    let mut out = Vec::new();
    walk(node, kind, stop_at, &mut out);
    out
}

/// Resolve a column reference's table scope: a qualifier (table alias/name) looks itself up in
/// `scope`, falling back to its own (normed) text if it isn't a known alias — still a concrete,
/// non-guessed answer (the qualifier IS syntactically a real name, just not one this statement's
/// FROM/JOIN happens to bind — e.g. a linked-server or cross-database prefix). No qualifier -> every
/// table currently in scope, deduped and `\x1f`-joined — the store resolves exact/ambiguous/
/// unresolved from that set by checking real column membership; the walk never decides that itself.
/// Empty scope (couldn't determine ANY table context) -> None: honest omission, not a guess.
fn scope_receiver(scope: &HashMap<String, String>, qualifier: Option<&str>) -> Option<String> {
    match qualifier {
        Some(q) => Some(scope.get(&norm(q)).cloned().unwrap_or_else(|| norm(q))),
        None => {
            let mut names: Vec<&str> = scope.values().map(String::as_str).collect();
            names.sort();
            names.dedup();
            (!names.is_empty()).then(|| names.join("\u{1f}"))
        }
    }
}

/// L3.4 (CTE lineage) — descend through single-named-child wrapper nodes (postgres's
/// `a_expr` -> `a_expr` -> `c_expr` precedence-climbing chain; a no-op single hop for plsql's
/// `expression` -> `referenced_element`) until landing on a node of `leaf_kind`, or `None` if
/// anything along the way has more than one named child (a binary operator, function call, `*`,
/// etc. — not a bare passthrough). A CTE's select-list value is always wrapped this way even for a
/// plain column, so this is the only way to tell "just a column" apart from "an expression that
/// happens to contain one".
fn bare_leaf<'t>(mut n: Node<'t>, leaf_kind: &str) -> Option<Node<'t>> {
    loop {
        if n.kind() == leaf_kind {
            return Some(n);
        }
        if n.named_child_count() != 1 {
            return None;
        }
        n = n.named_child(0).unwrap();
    }
}

/// Emit one column-reference `CallSite`. Deliberately NOT routed through `push_call` — its
/// builtin-name filter exists for function names, not column names, and would be a category error
/// here (a column genuinely named `count` or `left` is common and not a builtin call).
fn push_column_ref(
    out: &mut ParsedFile,
    raw_name: &str,
    access: &str,
    receiver_class: Option<String>,
    line: usize,
    enclosing: &str,
) {
    let Some(receiver_class) = receiver_class else { return };
    let name = norm(raw_name);
    if name.is_empty() {
        return;
    }
    out.calls.push(CallSite {
        name,
        kind: access.into(),
        line,
        enclosing: enclosing.to_string(),
        receiver_class: Some(receiver_class),
    });
}

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

/// L3.3 — nested-statement boundaries a DML column-ref/table-scope walk must stop at, so a
/// subquery's own tables/columns never leak into the enclosing statement's scope. `subquery` is
/// its own wrapper node (its `select` and that select's `from` are SIBLINGS under `subquery`, the
/// same sibling shape the top-level `statement` uses) — without it in this list, a walk that
/// correctly stops at `select` still recurses into `subquery` -> `from` and leaks the inner
/// table(s) into the outer scope (caught live by `tsql_subquery_gets_its_own_scope_not_the_outer_ones`).
const DML_STOP: &[&str] = &["subquery", "select", "update", "insert", "delete"];

/// L3.3 — alias/name -> real (bare, schema-dropped) table name for every `relation` under a tsql
/// FROM clause's subtree (base table + every JOIN), plus a self-mapped entry for DELETE's bare
/// target (a direct `object_reference`, no `relation` wrapper). Bare, schema-dropped, because
/// that's exactly what Phase A stores as a column's `parent_class` — this map's values must match
/// it exactly for scope resolution to find real column defs later.
fn tsql_table_scope(node: Node, src: &[u8]) -> HashMap<String, String> {
    let mut scope = HashMap::new();
    for rel in find_descendants_scoped(node, "relation", DML_STOP) {
        if let Some(or) = find_child(rel, "object_reference") {
            if let Some(nm) = or.child_by_field_name("name") {
                let table = norm(text(nm, src));
                scope.insert(table.clone(), table.clone());
                if let Some(alias) = rel.child_by_field_name("alias") {
                    scope.insert(norm(text(alias, src)), table);
                }
            }
        }
    }
    if scope.is_empty() {
        // DELETE FROM t (no relation wrapper) or UPDATE t (relation wraps it, already handled
        // above) — check for a bare object_reference as the sole remaining shape.
        if let Some(or) = find_child(node, "object_reference") {
            if let Some(nm) = or.child_by_field_name("name") {
                let table = norm(text(nm, src));
                scope.insert(table.clone(), table);
            }
        }
    }
    scope
}

/// L3.3 — a tsql `field` node's own column name plus its optional qualifier (the `name:` of a
/// nested `object_reference`, e.g. `o` in `o.status`).
fn tsql_field_parts<'a>(field: Node, src: &'a [u8]) -> (Option<&'a str>, Option<&'a str>) {
    let qualifier = find_child(field, "object_reference")
        .and_then(|or| or.child_by_field_name("name"))
        .map(|n| text(n, src));
    let column = field.child_by_field_name("name").map(|n| text(n, src));
    (qualifier, column)
}

/// L3.3 — every column reference (`field` node) under `node`, resolved against `scope`, emitted
/// as `access` ("read"/"write"). Stops at nested-statement boundaries (see
/// `find_descendants_scoped`) so a subquery's fields aren't misattributed to this scope.
/// `find_descendants_scoped` only matches DESCENDANTS, never `node` itself — include it directly
/// when the caller passes a bare `field` node (e.g. an UPDATE assignment's `right:` when it's an
/// unqualified column copy, `SET total = price`, which the grammar doesn't wrap in a container).
fn push_tsql_field_refs(
    out: &mut ParsedFile,
    node: Node,
    src: &[u8],
    scope: &HashMap<String, String>,
    access: &str,
    enclosing: &str,
) {
    let mut fields = find_descendants_scoped(node, "field", DML_STOP);
    if node.kind() == "field" {
        fields.insert(0, node);
    }
    for f in fields {
        let (qualifier, column) = tsql_field_parts(f, src);
        let Some(col) = column else { continue };
        push_column_ref(
            out,
            col,
            access,
            scope_receiver(scope, qualifier),
            f.start_position().row + 1,
            enclosing,
        );
    }
}

/// L3.4 (CTE lineage) — for every `cte` child of `stmt` (`WITH name[(cols)] AS (SELECT ...)`,
/// comma-chained siblings under the same `statement`), its output-column projections: output name
/// -> (real table, real column), one entry per select-list item that's a direct, untransformed
/// passthrough of a real table's column (bare `col`, `t.col`, or `t.col AS alias`) — resolved
/// against the CTE body's OWN table scope (`tsql_table_scope`, the same helper the body's own
/// SELECT already uses). An explicit column list (`WITH x(a,b) AS (...)`) overrides the derived
/// output names positionally. Anything else (window function, aggregate, expression, `*`, or a body
/// scope ambiguous between two tables) has no entry — its outer references stay unresolved exactly
/// as they were before this feature existed; never guess.
/// ponytail: keyed by CTE name only (no statement-local scoping) — two same-named CTEs with
/// different meanings in the same file collide (first one found wins). Rare in practice (repo-wide
/// collision across whole DIFFERENT files is already fine, this only bites two WITH-blocks in one
/// file reusing a generic name like `tmp`); revisit only if a real corpus hits it.
fn tsql_cte_projections(stmt: Node, src: &[u8]) -> HashMap<String, HashMap<String, (String, String)>> {
    let mut out = HashMap::new();
    let mut cursor = stmt.walk();
    for cte in stmt.children(&mut cursor).filter(|c| c.kind() == "cte") {
        let Some(name_node) = cte.child(0) else { continue };
        if name_node.kind() != "identifier" {
            continue;
        }
        let cte_name = norm(text(name_node, src));
        let explicit_cols: Vec<String> = {
            let mut c2 = cte.walk();
            cte.children_by_field_name("argument", &mut c2).map(|n| norm(text(n, src))).collect()
        };
        let Some(body) = find_child(cte, "statement") else { continue };
        let Some(body_select) = find_child(body, "select") else { continue };
        let body_scope = find_child(body, "from").map(|f| tsql_table_scope(f, src)).unwrap_or_default();
        let Some(sel_expr) = find_child(body_select, "select_expression") else { continue };
        let mut proj = HashMap::new();
        let mut c3 = sel_expr.walk();
        let terms = sel_expr.children(&mut c3).filter(|c| c.kind() == "term");
        for (idx, term) in terms.enumerate() {
            let value = term.child_by_field_name("value");
            let alias = term.child_by_field_name("alias").map(|n| norm(text(n, src)));
            let derived_name = value
                .filter(|v| v.kind() == "field")
                .and_then(|v| tsql_field_parts(v, src).1)
                .map(norm);
            let out_name = explicit_cols.get(idx).cloned().or(alias).or(derived_name);
            let (Some(out_name), Some(value)) = (out_name, value) else { continue };
            if value.kind() != "field" {
                continue; // expression/function/window/*, not a bare passthrough
            }
            let (qualifier, column) = tsql_field_parts(value, src);
            let Some(column) = column else { continue };
            let Some(real_table) = scope_receiver(&body_scope, qualifier) else { continue };
            if real_table.contains('\u{1f}') {
                continue; // ambiguous underlying table, never guess
            }
            proj.insert(out_name, (real_table, norm(column)));
        }
        if !proj.is_empty() {
            out.entry(cte_name).or_insert(proj);
        }
    }
    out
}

fn walk_tsql(node: Node, src: &[u8], out: &mut ParsedFile, enclosing: &str, in_error: bool) {
    let in_error = entering_error(node, in_error);
    if node.kind() == "statement" {
        for (k, v) in tsql_cte_projections(node, src) {
            out.cte_columns.entry(k).or_insert(v);
        }
    }
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
        // L3.2: `CREATE TABLE [schema.]name (col type ..., col type ..., ...)` -> a `table` def
        // plus one `column` def per column, parent_class = table name (same pattern Oracle package
        // members already use). `column_definition` carries a `name:` field directly — as clean as
        // the procedure-param walk this mirrors.
        "create_table" => {
            if let Some(or) = find_child(node, "object_reference") {
                let (schema, name) = obj_ref_parts(or, src);
                if let Some(nm) = name {
                    let d = mk_def(nm, "table", schema, node, src);
                    let table_name = d.name.clone();
                    def_name = Some(table_name.clone());
                    out.defs.push(d);
                    if let Some(cols) = find_child(node, "column_definitions") {
                        walk_children(cols, |cd| {
                            if cd.kind() == "column_definition" {
                                if let Some(cn) = cd.child_by_field_name("name") {
                                    out.defs.push(mk_def(
                                        text(cn, src),
                                        "column",
                                        Some(&table_name),
                                        cd,
                                        src,
                                    ));
                                }
                            }
                        });
                    }
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
        // L3.3: SELECT list + JOIN-ON + WHERE column references, all reads. `from` is a SIBLING
        // of `select`, both under `statement` for a top-level query OR under `subquery` for a
        // nested one (same sibling shape either way — `subquery` is why it's also in `DML_STOP`;
        // this arm is what actually gives a nested subquery its own correctly-scoped extraction
        // once the outer walk's `DML_STOP` boundary is crossed by the normal recursion below).
        // Matched here the same way DELETE is below, guarded on having a `select` child rather
        // than matching `select` directly, so both siblings are reachable without a parent-pointer
        // walk. Two separate extraction calls, each rooted at the right node (never at `statement`/
        // `subquery` itself, which would incorrectly trip the nested-subquery stop-list on the
        // select being processed right now).
        "statement" | "subquery" if find_child(node, "select").is_some() => {
            let select = find_child(node, "select").unwrap();
            let from = find_child(node, "from");
            let scope = from.map(|f| tsql_table_scope(f, src)).unwrap_or_default();
            push_tsql_field_refs(out, select, src, &scope, "read", enclosing);
            if let Some(from) = from {
                push_tsql_field_refs(out, from, src, &scope, "read", enclosing);
            }
        }
        // UPDATE: SET target = write; SET source expression + WHERE = reads. A plain (non-FROM)
        // UPDATE's scope is exactly its one target table, so an unqualified SET target correctly
        // resolves via the same `scope_receiver` logic every other column ref uses — no special
        // casing needed for "the" table being updated.
        "update" if !in_error => {
            let scope = tsql_table_scope(node, src);
            for assign in find_descendants_scoped(node, "assignment", DML_STOP) {
                if let Some(left) = assign.child_by_field_name("left") {
                    if left.kind() == "field" {
                        let (qualifier, column) = tsql_field_parts(left, src);
                        if let Some(col) = column {
                            push_column_ref(
                                out,
                                col,
                                "write",
                                scope_receiver(&scope, qualifier),
                                left.start_position().row + 1,
                                enclosing,
                            );
                        }
                    }
                }
                if let Some(right) = assign.child_by_field_name("right") {
                    push_tsql_field_refs(out, right, src, &scope, "read", enclosing);
                }
            }
            if let Some(w) = find_child(node, "where") {
                push_tsql_field_refs(out, w, src, &scope, "read", enclosing);
            }
        }
        // INSERT: the column list is a write to the single target table — no alias resolution
        // needed (INSERT never joins). Values themselves are literals/expressions, not tracked.
        "insert" if !in_error => {
            if let Some(or) = find_child(node, "object_reference") {
                if let Some(nm) = or.child_by_field_name("name") {
                    let table = norm(text(nm, src));
                    let mut scope = HashMap::new();
                    scope.insert(table.clone(), table);
                    if let Some(cols) = find_child(node, "list") {
                        walk_children(cols, |c| {
                            if c.kind() == "column" {
                                if let Some(id) = c.named_child(0) {
                                    push_column_ref(
                                        out,
                                        text(id, src),
                                        "write",
                                        scope_receiver(&scope, None),
                                        id.start_position().row + 1,
                                        enclosing,
                                    );
                                }
                            }
                        });
                    }
                }
            }
        }
        // DELETE: `delete` and `from` are SIBLINGS under `statement` (unlike SELECT/UPDATE, where
        // FROM is nested inside the statement node) — matched here, guarded on having a `delete`
        // child, rather than on `delete` itself, so no parent-pointer walk is needed. Only a WHERE
        // to read from; the target table is the bare `object_reference` `tsql_table_scope`'s
        // fallback branch handles (no `relation` wrapper for a plain DELETE).
        "statement" if find_child(node, "delete").is_some() => {
            if let Some(from) = find_child(node, "from") {
                let scope = tsql_table_scope(from, src);
                if let Some(w) = find_child(from, "where") {
                    push_tsql_field_refs(out, w, src, &scope, "read", enclosing);
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

/// L3.3 — plsql's own nested-statement boundary list. `scalar_subquery` is a DIFFERENT node kind
/// from `sql_statement_select` for an inline nested query (unlike postgres, where the same
/// `simple_select` node appears at both nesting levels) — every statement kind is included for
/// consistency/safety even though only SELECT genuinely nests in practice.
const PLSQL_DML_STOP: &[&str] = &[
    "scalar_subquery",
    "sql_statement_select",
    "sql_statement_update",
    "sql_statement_insert",
    "sql_statement_delete",
];

/// L3.3 — one `referenced_element` node's `ref_name` plus optional `ref_name_parent` qualifier.
/// The SAME node kind is used for both table names (in `table_list`) and column references (in
/// `select_list`/`where_clause`/etc) — callers scope their search to the right subtree so the two
/// never get conflated (see `plsql_table_scope` vs `push_plsql_referenced_element_refs`).
fn plsql_referenced_element_parts<'a>(re: Node, src: &'a [u8]) -> (Option<&'a str>, Option<&'a str>) {
    (
        re.child_by_field_name("ref_name_parent").map(|n| text(n, src)),
        re.child_by_field_name("ref_name").map(|n| text(n, src)),
    )
}

/// L3.3 — alias/name -> real (bare) table name for every `referenced_element` under a plsql
/// `table_list` (searched scoped to that subtree specifically, never the whole statement, so a
/// column reference elsewhere is never mistaken for a table). Covers both shapes, confirmed via
/// grammar dump: a plain `table_list_element (referenced_element ...) alias: (identifier)` (no
/// join — one alias field, safe to read directly off the parent), and a `join_clause`'s TWO
/// `referenced_element`s + TWO `alias:` fields as FLAT SIBLINGS of each other under the SAME
/// parent — `child_by_field_name` only ever returns the FIRST match for a repeated field name, so
/// for the join shape the alias must be found positionally (`next_alias_for`), not via
/// `parent().child_by_field_name("alias")`, or both aliases resolve to the first table.
fn plsql_table_scope(table_list: Node, src: &[u8]) -> HashMap<String, String> {
    let mut scope = HashMap::new();
    let stop: Vec<&str> = PLSQL_DML_STOP.iter().copied().chain(std::iter::once("expression")).collect();
    for re in find_descendants_scoped(table_list, "referenced_element", &stop) {
        let Some(nm) = re.child_by_field_name("ref_name") else { continue };
        let table = norm(text(nm, src));
        scope.insert(table.clone(), table.clone());
        if let Some(alias) = next_alias_for(re) {
            scope.insert(norm(text(alias, src)), table);
        }
    }
    scope
}

/// Find the `alias:`-field sibling immediately following `re` under `re`'s parent, stopping (and
/// returning `None`) if another `referenced_element` is reached first — i.e. an unaliased table.
/// Needed because a `join_clause` has two `referenced_element`s sharing one parent with two
/// same-named `alias` fields, which `Node::child_by_field_name` cannot disambiguate by itself.
fn next_alias_for(re: Node) -> Option<Node> {
    let parent = re.parent()?;
    let mut cursor = parent.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    let mut found = false;
    loop {
        if found {
            if cursor.field_name() == Some("alias") {
                return Some(cursor.node());
            }
            if cursor.node().kind() == "referenced_element" {
                return None;
            }
        } else if cursor.node().id() == re.id() {
            found = true;
        }
        if !cursor.goto_next_sibling() {
            return None;
        }
    }
}

/// L3.3 — every column reference (`referenced_element` node) under `node`, resolved against
/// `scope`, emitted as `access`. Stops at nested-statement boundaries (`PLSQL_DML_STOP`) so a
/// subquery's fields aren't misattributed to this scope. Caller must scope `node` to an
/// expression context (select_list, where_clause, ...) — never `table_list`, whose
/// `referenced_element`s are table names, not columns.
fn push_plsql_referenced_element_refs(
    out: &mut ParsedFile,
    node: Node,
    src: &[u8],
    scope: &HashMap<String, String>,
    access: &str,
    enclosing: &str,
) {
    for re in find_descendants_scoped(node, "referenced_element", PLSQL_DML_STOP) {
        // A function call's callee name is ALSO a bare `referenced_element` — `ROUND(total, 2)`
        // parses as `(ref_call (referenced_element ref_name: "ROUND") (parameter ...))` — so the
        // callee-name node is the direct child of `ref_call`, unlike its arguments (nested several
        // levels down through parameter/expression). Skip it here; `walk_plsql`'s own `"ref_call"`
        // arm already extracts it as a real call, and re-emitting it as a column read would
        // misclassify every builtin/user function name (ROUND, NULLIF, MONTHS_BETWEEN, ...) used
        // in a SQL expression as a nonexistent column.
        if re.parent().map(|p| p.kind()) == Some("ref_call") {
            continue;
        }
        let (qualifier, column) = plsql_referenced_element_parts(re, src);
        let Some(col) = column else { continue };
        push_column_ref(
            out,
            col,
            access,
            scope_receiver(scope, qualifier),
            re.start_position().row + 1,
            enclosing,
        );
    }
}

pub fn parse_plsql(src: &str) -> anyhow::Result<ParsedFile> {
    let pre = strip_sqlplus_lines(src);
    let tree = tree_for(&pre, tree_sitter_plsql::language(), "plsql")?;
    let mut out = ParsedFile::default();
    walk_plsql(tree.root_node(), pre.as_bytes(), &mut out, "<module>", None, false);
    out.symbolless_ok = !tree.root_node().has_error();
    Ok(out)
}

/// L3.4 (CTE lineage) — plsql counterpart to `tsql_cte_projections`/`pg_cte_projections` (see the
/// former's doc comment for the overall projection design). Unlike either, a plsql `with_clause`
/// has no per-CTE wrapper node at all — EVERY CTE's pieces (`query_name`, optional
/// `( referenced_element_repeat )` column list, `kw_as`, `(`, body pieces, `)`) sit as FLAT SIBLINGS
/// directly under `with_clause`, chained by a `,` between CTEs (confirmed via grammar dump: two
/// CTEs produce one `with_clause` with both bodies' pieces interleaved in source order, no grouping
/// node to recurse into). So this walks the flat child list with a manual index instead of
/// recursing per-CTE, using `kw_as`/`(`/`)` as the only structural landmarks.
///
/// Also does double duty pushing each CTE body's OWN column reads to `out` (a `read` access,
/// exactly like `"sql_statement_select"`'s own handling) — unlike tsql/postgres, where the body is
/// a nested `statement`/`SelectStmt` the generic recursion visits and extracts on its own, plsql's
/// flattened shape means nothing else ever visits these pieces; without this the body's own reads
/// (e.g. a CTE's WHERE clause) were never extracted at all, CTE or no CTE-lineage feature.
fn plsql_walk_with_clause(with_clause: Node, src: &[u8], out: &mut ParsedFile, enclosing: &str) {
    let mut projections: HashMap<String, HashMap<String, (String, String)>> = HashMap::new();
    let children: Vec<(Option<&'static str>, Node)> = {
        let mut c = with_clause.walk();
        let mut v = Vec::new();
        if c.goto_first_child() {
            loop {
                v.push((c.field_name(), c.node()));
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
        v
    };
    let mut i = 0;
    while i < children.len() {
        if children[i].0 != Some("query_name") {
            i += 1;
            continue;
        }
        let cte_name = norm(text(children[i].1, src));
        i += 1;
        let mut explicit_cols = Vec::new();
        if i < children.len() && children[i].1.kind() == "(" {
            i += 1;
            if i < children.len() && children[i].1.kind() == "referenced_element_repeat" {
                let mut cw = children[i].1.walk();
                explicit_cols = children[i]
                    .1
                    .named_children(&mut cw)
                    .filter(|n| n.kind() == "referenced_element")
                    .filter_map(|n| n.child_by_field_name("ref_name"))
                    .map(|n| norm(text(n, src)))
                    .collect();
                i += 1;
            }
            if i < children.len() && children[i].1.kind() == ")" {
                i += 1;
            }
        }
        if i < children.len() && children[i].1.kind() == "kw_as" {
            i += 1;
        }
        if !(i < children.len() && children[i].1.kind() == "(") {
            continue; // unexpected shape (e.g. an error node) — skip this CTE defensively
        }
        i += 1;
        let (mut select_list, mut table_list, mut where_clause) = (None, None, None);
        while i < children.len() && children[i].1.kind() != ")" {
            match children[i].1.kind() {
                "select_list" => select_list = Some(children[i].1),
                "table_list" => table_list = Some(children[i].1),
                "where_clause" => where_clause = Some(children[i].1),
                _ => {}
            }
            i += 1;
        }
        if i < children.len() {
            i += 1; // consume the body's closing ")"
        }
        let (Some(select_list), Some(table_list)) = (select_list, table_list) else { continue };
        let body_scope = plsql_table_scope(table_list, src);

        // Body's own reads — the extraction the generic recursion never reaches for a CTE body
        // (see doc comment above).
        push_plsql_referenced_element_refs(out, select_list, src, &body_scope, "read", enclosing);
        if let Some(w) = where_clause {
            push_plsql_referenced_element_refs(out, w, src, &body_scope, "read", enclosing);
        }

        let mut proj = HashMap::new();
        let mut cw2 = select_list.walk();
        let elements: Vec<Node> =
            select_list.named_children(&mut cw2).filter(|n| n.kind() == "select_list_element").collect();
        for (idx, elem) in elements.into_iter().enumerate() {
            let alias = find_child(elem, "identifier").map(|n| norm(text(n, src)));
            let bare = find_child(elem, "expression").and_then(|v| bare_leaf(v, "referenced_element"));
            let derived_name = bare.and_then(|re| plsql_referenced_element_parts(re, src).1).map(norm);
            let out_name = explicit_cols.get(idx).cloned().or(alias).or(derived_name);
            let (Some(out_name), Some(re)) = (out_name, bare) else { continue };
            let (qualifier, column) = plsql_referenced_element_parts(re, src);
            let Some(column) = column else { continue };
            let Some(real_table) = scope_receiver(&body_scope, qualifier) else { continue };
            if real_table.contains('\u{1f}') {
                continue; // ambiguous underlying table, never guess
            }
            proj.insert(out_name, (real_table, norm(column)));
        }
        if !proj.is_empty() {
            projections.entry(cte_name).or_insert(proj);
        }
    }
    for (k, v) in projections {
        out.cte_columns.entry(k).or_insert(v);
    }
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
        // L3.2: `CREATE TABLE name (col type ..., ...)` -> a `table` def plus one `column` def per
        // column. Both `table_name:` and each `table_column_definition`'s `column_name:` are direct
        // field-named nodes — as clean as the procedure-param walk this mirrors.
        "create_table" => {
            if let Some(nm) = node.child_by_field_name("table_name") {
                let d = mk_def(text(nm, src), "table", None, node, src);
                let table_name = d.name.clone();
                def_name = Some(table_name.clone());
                out.defs.push(d);
                walk_children(node, |te| {
                    if te.kind() == "table_element" {
                        if let Some(cd) = find_child(te, "table_column_definition") {
                            if let Some(cn) = cd.child_by_field_name("column_name") {
                                out.defs.push(mk_def(
                                    text(cn, src),
                                    "column",
                                    Some(&table_name),
                                    te,
                                    src,
                                ));
                            }
                        }
                    }
                });
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
        // L3.4 (CTE lineage): each CTE body's own reads, plus the projection map used to resolve
        // the OUTER query's references to it (built and applied in `plsql_walk_with_clause`). The
        // outer query's own select_list/table_list/where_clause are UNAFFECTED — they're direct
        // children of `sql_statement_select` itself (siblings of `with_clause`, not inside it), so
        // that arm below reaches them exactly as it did before this feature existed.
        "with_clause" if !in_error => {
            plsql_walk_with_clause(node, src, out, enclosing);
        }
        // L3.3: SELECT list + JOIN-ON + WHERE column references, all reads. Matched on both
        // `sql_statement_select` (top-level) and `scalar_subquery` (a nested query — a DIFFERENT
        // node kind here, unlike postgres where the same inner node appears at both levels) so
        // each gets its own correctly-scoped extraction; `PLSQL_DML_STOP` (which includes
        // `scalar_subquery`) keeps a subquery's own fields from leaking into the outer scope.
        "sql_statement_select" | "scalar_subquery" if !in_error => {
            let scope = find_child(node, "table_list")
                .map(|tl| plsql_table_scope(tl, src))
                .unwrap_or_default();
            if let Some(sl) = find_child(node, "select_list") {
                push_plsql_referenced_element_refs(out, sl, src, &scope, "read", enclosing);
            }
            if let Some(w) = find_child(node, "where_clause") {
                push_plsql_referenced_element_refs(out, w, src, &scope, "read", enclosing);
            }
            // JOIN-ON predicate fields live inside table_list's join_clause -> expression, a
            // sibling of the table references `plsql_table_scope` deliberately stops before.
            if let Some(tl) = find_child(node, "table_list") {
                for jc in find_descendants_scoped(tl, "join_clause", PLSQL_DML_STOP) {
                    if let Some(on_expr) = find_child(jc, "expression") {
                        push_plsql_referenced_element_refs(out, on_expr, src, &scope, "read", enclosing);
                    }
                }
            }
        }
        // UPDATE: SET target = write; SET source expression + WHERE = reads. Target table is a
        // direct child (no table_list wrapper the way SELECT has), so its scope is exactly the
        // one table being updated.
        "sql_statement_update" if !in_error => {
            let mut scope = HashMap::new();
            if let Some(re) = find_child(node, "referenced_element") {
                if let Some(nm) = re.child_by_field_name("ref_name") {
                    let table = norm(text(nm, src));
                    scope.insert(table.clone(), table);
                }
            }
            for elems in find_descendants_scoped(node, "update_set_clause_elements", PLSQL_DML_STOP) {
                if let Some(target) = find_child(elems, "referenced_element") {
                    if let Some(nm) = target.child_by_field_name("ref_name") {
                        push_column_ref(
                            out,
                            text(nm, src),
                            "write",
                            scope_receiver(&scope, None),
                            target.start_position().row + 1,
                            enclosing,
                        );
                    }
                }
                if let Some(expr) = find_child(elems, "expression") {
                    push_plsql_referenced_element_refs(out, expr, src, &scope, "read", enclosing);
                }
            }
            if let Some(w) = find_child(node, "where_clause") {
                push_plsql_referenced_element_refs(out, w, src, &scope, "read", enclosing);
            }
        }
        // INSERT: the column list is a write to the single target table.
        "sql_statement_insert" if !in_error => {
            if let Some(re) = find_descendants_scoped(node, "referenced_element", PLSQL_DML_STOP)
                .into_iter()
                .next()
            {
                if let Some(nm) = re.child_by_field_name("ref_name") {
                    let table = norm(text(nm, src));
                    let mut scope = HashMap::new();
                    scope.insert(table.clone(), table);
                    for col in find_descendants_scoped(node, "insert_column", PLSQL_DML_STOP) {
                        if let Some(id) = col.named_child(0) {
                            push_column_ref(
                                out,
                                text(id, src),
                                "write",
                                scope_receiver(&scope, None),
                                id.start_position().row + 1,
                                enclosing,
                            );
                        }
                    }
                }
            }
        }
        // DELETE: target table + WHERE, both direct children.
        "sql_statement_delete" if !in_error => {
            let mut scope = HashMap::new();
            if let Some(re) = find_child(node, "referenced_element") {
                if let Some(nm) = re.child_by_field_name("ref_name") {
                    let table = norm(text(nm, src));
                    scope.insert(table.clone(), table);
                }
            }
            if let Some(w) = find_child(node, "where_clause") {
                push_plsql_referenced_element_refs(out, w, src, &scope, "read", enclosing);
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

/// L3.3 — postgres's own nested-statement boundary list. A different vocabulary from tsql's
/// `DML_STOP`: an inline subquery is wrapped as `select_with_parens` (NOT the bare `SelectStmt`
/// its own top-level statement uses), so tsql's list wouldn't stop at it here.
const PG_DML_STOP: &[&str] = &["select_with_parens", "SelectStmt", "UpdateStmt", "InsertStmt", "DeleteStmt"];

/// L3.3 — alias/name -> real (bare) table name for every `relation_expr` under a postgres
/// FROM/JOIN subtree, or UPDATE/DELETE's direct target. `relation_expr` (not `table_ref`) is the
/// collection target deliberately: a `table_ref` for a JOIN wraps `joined_table`, which itself
/// contains TWO MORE `table_ref`s (one per side) — `table_ref` self-nests and a naive "stop at
/// first match" collector would find only the outer wrapper and miss both real tables (caught live
/// by `pg_join_widens_unqualified_scope`, which returned only one of the two joined tables before
/// this fix). `relation_expr` never self-nests, so it's the genuinely leaf-like target; its alias
/// (if any) is found by walking up ONE level to its own immediate parent (`table_ref` for a
/// FROM/JOIN entry, `relation_expr_opt_alias` for a bare UPDATE/DELETE target) rather than
/// searching the whole subtree — both shapes are handled uniformly this way, no separate
/// bare-target fallback branch needed.
fn pg_table_scope(node: Node, src: &[u8]) -> HashMap<String, String> {
    let mut scope = HashMap::new();
    for re in find_descendants_scoped(node, "relation_expr", PG_DML_STOP) {
        let Some(qn) = find_child(re, "qualified_name") else { continue };
        let Some(nm) = qualified_name_object(qn, src) else { continue };
        let table = norm(nm);
        scope.insert(table.clone(), table.clone());
        if let Some(parent) = re.parent() {
            if let Some(alias_clause) = find_child(parent, "alias_clause")
                .or_else(|| find_child(parent, "opt_alias_clause").and_then(|oac| find_child(oac, "alias_clause")))
            {
                if let Some(acid) = find_child(alias_clause, "ColId") {
                    scope.insert(norm(text(acid, src)), table);
                }
            }
        }
    }
    scope
}

/// L3.3 — a postgres `columnref` node's own column name plus its optional qualifier. `columnref`'s
/// first child (`ColId`) is the BARE column name when unqualified, or the QUALIFIER when an
/// `indirection` follows (the actual column name then lives in `indirection`'s `attr_name`). Takes
/// each matched node's own full text span rather than reaching for a specific leaf kind inside it
/// (`identifier` vs `unreserved_keyword` — a column literally named `name` tags differently than
/// one named `status`, and the span text is correct either way).
fn pg_columnref_parts<'a>(columnref: Node, src: &'a [u8]) -> (Option<&'a str>, Option<&'a str>) {
    let Some(colid) = find_child(columnref, "ColId") else {
        return (None, None);
    };
    match find_descendant(columnref, "attr_name") {
        Some(attr) => (Some(text(colid, src)), Some(text(attr, src))),
        None => (None, Some(text(colid, src))),
    }
}

/// L3 — the real object name from a postgres `qualified_name` node. `ColId` alone is only correct
/// when the name is unqualified: a schema-qualified name (`Sales.BuyingGroups`) uses the SAME
/// `ColId` + optional `indirection` shape `columnref` does — `ColId` holds the FIRST part (the
/// schema), and the real object name is in `indirection`'s (last, for a 3+-part name)
/// `indirection_el -> attr_name`. Found live: Phase A's own CREATE TABLE handling had exactly this
/// bug for schema-qualified tables — extracting "sales" as the table name from `CREATE TABLE
/// Sales.BuyingGroups`, surfaced dogfooding the real Wide World Importers schema (every table
/// there is schema-qualified), not by any fixture — none of this session's own hand-written test
/// SQL happened to use a schema prefix, which is exactly why a real corpus matters.
fn qualified_name_object<'a>(qn: Node, src: &'a [u8]) -> Option<&'a str> {
    if let Some(indirection) = find_child(qn, "indirection") {
        let mut c = indirection.walk();
        let last = indirection
            .named_children(&mut c)
            .filter(|el| el.kind() == "indirection_el")
            .filter_map(|el| find_child(el, "attr_name"))
            .last();
        if let Some(attr) = last {
            return Some(text(attr, src));
        }
    }
    find_child(qn, "ColId").map(|c| text(c, src))
}

/// L3.3 — every column reference (`columnref` node) under `node`, resolved against `scope`,
/// emitted as `access`. Stops at nested-statement boundaries (`PG_DML_STOP`) so a subquery's
/// fields aren't misattributed to this scope.
fn push_pg_columnref_refs(
    out: &mut ParsedFile,
    node: Node,
    src: &[u8],
    scope: &HashMap<String, String>,
    access: &str,
    enclosing: &str,
) {
    for cr in find_descendants_scoped(node, "columnref", PG_DML_STOP) {
        let (qualifier, column) = pg_columnref_parts(cr, src);
        let Some(col) = column else { continue };
        push_column_ref(
            out,
            col,
            access,
            scope_receiver(scope, qualifier),
            cr.start_position().row + 1,
            enclosing,
        );
    }
}

/// L3.4 (CTE lineage) — postgres counterpart to `tsql_cte_projections` (see its doc comment for the
/// overall design). Different grammar shape: `with_clause -> cte_list -> common_table_expr`, each
/// with a `name` node (its own span IS the CTE name) and an optional `opt_name_list` explicit
/// column list. The body sits three wrapper layers below `common_table_expr`
/// (`PreparableStmt -> SelectStmt -> select_no_parens -> simple_select`) — the same chain the
/// `"simple_select"` walk arm reaches generically for the body's OWN extraction; this reaches it
/// explicitly to also read its scope for the projection.
fn pg_cte_projections(with_clause: Node, src: &[u8]) -> HashMap<String, HashMap<String, (String, String)>> {
    let mut out = HashMap::new();
    let Some(cte_list) = find_child(with_clause, "cte_list") else { return out };
    for cte in find_descendants_scoped(cte_list, "common_table_expr", &[]) {
        let Some(name_node) = find_child(cte, "name") else { continue };
        let cte_name = norm(text(name_node, src));
        let explicit_cols: Vec<String> = find_child(cte, "opt_name_list")
            .map(|onl| find_descendants_scoped(onl, "name", &[]).iter().map(|n| norm(text(*n, src))).collect())
            .unwrap_or_default();
        let Some(prep) = find_child(cte, "PreparableStmt") else { continue };
        let Some(select_stmt) = find_child(prep, "SelectStmt") else { continue };
        let Some(select_no_parens) = find_child(select_stmt, "select_no_parens") else { continue };
        let Some(body_select) = find_child(select_no_parens, "simple_select") else { continue };
        let body_scope = find_descendants_scoped(body_select, "from_clause", PG_DML_STOP)
            .into_iter()
            .next()
            .map(|f| pg_table_scope(f, src))
            .unwrap_or_default();
        let Some(opt_targets) = find_child(body_select, "opt_target_list") else { continue };
        let mut proj = HashMap::new();
        let targets = find_descendants_scoped(opt_targets, "target_el", &[]);
        for (idx, target_el) in targets.into_iter().enumerate() {
            let alias = find_child(target_el, "ColLabel").map(|n| norm(text(n, src)));
            let bare = find_child(target_el, "a_expr").and_then(|v| bare_leaf(v, "columnref"));
            let derived_name = bare.and_then(|cr| pg_columnref_parts(cr, src).1).map(norm);
            let out_name = explicit_cols.get(idx).cloned().or(alias).or(derived_name);
            let (Some(out_name), Some(cr)) = (out_name, bare) else { continue };
            let (qualifier, column) = pg_columnref_parts(cr, src);
            let Some(column) = column else { continue };
            let Some(real_table) = scope_receiver(&body_scope, qualifier) else { continue };
            if real_table.contains('\u{1f}') {
                continue; // ambiguous underlying table, never guess
            }
            proj.insert(out_name, (real_table, norm(column)));
        }
        if !proj.is_empty() {
            out.entry(cte_name).or_insert(proj);
        }
    }
    out
}

fn walk_pg(node: Node, src: &[u8], out: &mut ParsedFile, enclosing: &str, ps: &mut PgParsers, in_error: bool) {
    let in_error = entering_error(node, in_error);
    if node.kind() == "select_no_parens" {
        if let Some(wc) = find_child(node, "with_clause") {
            for (k, v) in pg_cte_projections(wc, src) {
                out.cte_columns.entry(k).or_insert(v);
            }
        }
    }
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
        // L3.2: `CREATE TABLE name (col type ..., ...)` -> a `table` def plus one `column` def per
        // column. `TableElementList` is left-recursive (not a flat sibling list like tsql's) so
        // `find_descendants` walks the whole subtree for every `columnDef`, not just direct
        // children — still exact (columnDef never nests another columnDef), just more code than
        // tsql/plsql's field-named lists.
        "CreateStmt" if find_child(node, "kw_table").is_some() => {
            if let Some(qn) = find_child(node, "qualified_name") {
                if let Some(nm) = qualified_name_object(qn, src) {
                    let d = mk_def(nm, "table", None, node, src);
                    let table_name = d.name.clone();
                    def_name = Some(table_name.clone());
                    out.defs.push(d);
                    for col in find_descendants(node, "columnDef") {
                        if let Some(cid) = find_child(col, "ColId") {
                            out.defs.push(mk_def(
                                text(cid, src),
                                "column",
                                Some(&table_name),
                                col,
                                src,
                            ));
                        }
                    }
                }
            }
        }
        // plain SQL calls in the outer file (SELECT setup(); triggers' EXECUTE FUNCTION; defaults)
        "func_application" if !in_error => {
            if let Some(fname) = find_child(node, "func_name") {
                pg_push_func(out, text(fname, src), node.start_position().row + 1, enclosing);
            }
        }
        // L3.3: SELECT list + JOIN-ON + WHERE column references, all reads. Matched on
        // `simple_select`, NOT `SelectStmt` — a top-level query is `SelectStmt -> select_no_parens
        // -> simple_select`, but an inline subquery is `select_with_parens -> select_no_parens ->
        // simple_select`, with NO `SelectStmt` node at that level at all. `simple_select` is the
        // one node kind present in both shapes, so matching on it (rather than trying to also
        // match `select_with_parens` separately) gives both the outer query and every nested
        // subquery their own correctly-scoped extraction uniformly. `push_pg_columnref_refs` stops
        // at `PG_DML_STOP` (which includes `select_with_parens`), so a subquery's own fields are
        // never double-counted into the outer scope.
        //
        // `from_clause` lookup deliberately uses `find_descendants_scoped` (stack-safe, respects
        // PG_DML_STOP), NOT the older `find_descendant` — that helper's stack-based DFS visits
        // children in REVERSE order (LIFO pop), so `find_descendant(select_stmt, "from_clause")`
        // could return a SUBQUERY's own nested from_clause instead of the outer statement's real
        // one, when the WHERE clause (containing the subquery) is examined before the FROM clause
        // is. Caught live by `pg_subquery_gets_its_own_scope_not_the_outer_ones`.
        "simple_select" if !in_error => {
            let scope = find_descendants_scoped(node, "from_clause", PG_DML_STOP)
                .into_iter()
                .next()
                .map(|f| pg_table_scope(f, src))
                .unwrap_or_default();
            push_pg_columnref_refs(out, node, src, &scope, "read", enclosing);
        }
        // UPDATE: SET target = write; SET source expression + WHERE = reads.
        "UpdateStmt" if !in_error => {
            let scope = pg_table_scope(node, src);
            for set_clause in find_descendants_scoped(node, "set_clause", PG_DML_STOP) {
                if let Some(target) = find_child(set_clause, "set_target") {
                    if let Some(cid) = find_child(target, "ColId") {
                        push_column_ref(
                            out,
                            text(cid, src),
                            "write",
                            scope_receiver(&scope, None),
                            cid.start_position().row + 1,
                            enclosing,
                        );
                    }
                }
                // the value expression is every other named child of set_clause besides set_target
                for child in set_clause.named_children(&mut set_clause.walk()) {
                    if child.kind() != "set_target" {
                        push_pg_columnref_refs(out, child, src, &scope, "read", enclosing);
                    }
                }
            }
            if let Some(w) = find_child(node, "where_or_current_clause") {
                push_pg_columnref_refs(out, w, src, &scope, "read", enclosing);
            }
        }
        // INSERT: the column list is a write to the single target table.
        "InsertStmt" if !in_error => {
            if let Some(it) = find_child(node, "insert_target") {
                if let Some(qn) = find_child(it, "qualified_name") {
                    if let Some(nm) = qualified_name_object(qn, src) {
                        let table = norm(nm);
                        let mut scope = HashMap::new();
                        scope.insert(table.clone(), table);
                        for item in find_descendants_scoped(node, "insert_column_item", PG_DML_STOP) {
                            if let Some(icid) = find_child(item, "ColId") {
                                push_column_ref(
                                    out,
                                    text(icid, src),
                                    "write",
                                    scope_receiver(&scope, None),
                                    icid.start_position().row + 1,
                                    enclosing,
                                );
                            }
                        }
                    }
                }
            }
        }
        // DELETE: target table + WHERE, both direct children (unlike tsql, no sibling split).
        "DeleteStmt" if !in_error => {
            let scope = pg_table_scope(node, src);
            if let Some(w) = find_child(node, "where_or_current_clause") {
                push_pg_columnref_refs(out, w, src, &scope, "read", enclosing);
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

/// EVERY descendant of `kind` (not just the first) — L3.2 needs this for `columnDef`, since
/// postgres's `TableElementList` is a left-recursive list (`TableElementList -> TableElementList
/// TableElement | TableElement`), not a flat sibling list the way tsql's `column_definitions` is.
/// Does not recurse into a matched node's own subtree (`columnDef` never nests another `columnDef`
/// in this grammar, so this is exact, not a heuristic bound).
fn find_descendants<'t>(node: Node<'t>, kind: &str) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == kind {
            out.push(n);
            continue;
        }
        let mut c = n.walk();
        // push in reverse so popping the stack visits children left-to-right, keeping `out` in
        // the same order columns appear in source (matters for readable, stable test output).
        for child in n.named_children(&mut c).collect::<Vec<_>>().into_iter().rev() {
            stack.push(child);
        }
    }
    out
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
    fn tsql_create_table_extracts_table_and_column_defs() {
        // L3.2: CREATE TABLE used to produce zero defs (Wave L2 scope). Now it's a `table` def
        // plus one `column` def per column, parent_class = table name.
        let p = parse_tsql("CREATE TABLE t (id INT PRIMARY KEY, status VARCHAR(20));\n").unwrap();
        assert!(p.calls.is_empty());
        let table = p.defs.iter().find(|d| d.name == "t" && d.kind == "table").expect("table def");
        assert!(table.parent_class.is_none());
        let id = p.defs.iter().find(|d| d.name == "id" && d.kind == "column").expect("id column");
        assert_eq!(id.parent_class.as_deref(), Some("t"));
        let status = p.defs.iter().find(|d| d.name == "status" && d.kind == "column").expect("status column");
        assert_eq!(status.parent_class.as_deref(), Some("t"));
        assert_eq!(p.defs.len(), 3, "table + 2 columns, nothing else");
    }

    // ---- L3.3: tsql DML column references ----

    fn find_col<'a>(p: &'a ParsedFile, name: &str, access: &str) -> &'a CallSite {
        p.calls
            .iter()
            .find(|c| c.name == name && c.kind == access)
            .unwrap_or_else(|| panic!("no {access} ref to {name} in {:?}", p.calls))
    }

    #[test]
    fn tsql_select_qualified_and_unqualified_reads() {
        let p = parse_tsql(
            "SELECT o.status, total FROM orders o WHERE o.id = 5 AND total > 0;\n",
        )
        .unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        // `total` is unqualified with exactly one table in scope -> that table, not ambiguous.
        // (Multiple appearances of `total` — SELECT list and WHERE — both resolve the same way.)
        assert!(p
            .calls
            .iter()
            .filter(|c| c.name == "total" && c.kind == "read")
            .all(|c| c.receiver_class.as_deref() == Some("orders")));
    }

    #[test]
    fn tsql_join_widens_unqualified_scope_for_ambiguity_check_later() {
        // The walk never decides exact/ambiguous itself — it emits the full candidate set and the
        // store resolves it. Two tables in scope -> both names present, delimiter-joined.
        let p = parse_tsql(
            "SELECT status FROM orders o JOIN order_items oi ON o.id = oi.order_id WHERE status = 'open';\n",
        )
        .unwrap();
        let scope = find_col(&p, "status", "read").receiver_class.clone().unwrap();
        let mut names: Vec<&str> = scope.split('\u{1f}').collect();
        names.sort();
        assert_eq!(names, vec!["order_items", "orders"]);
        // Qualified refs in the JOIN's ON clause resolve to their own single table, unaffected.
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "order_id", "read").receiver_class.as_deref(), Some("order_items"));
    }

    #[test]
    fn tsql_update_set_target_is_write_where_is_read() {
        let p = parse_tsql("UPDATE orders SET status = 'closed', total = price WHERE id = 5;\n").unwrap();
        assert_eq!(find_col(&p, "status", "write").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "total", "write").receiver_class.as_deref(), Some("orders"));
        // `price` on the right of `total = price` is a read (copying one column into another).
        assert_eq!(find_col(&p, "price", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn tsql_insert_column_list_is_write() {
        let p = parse_tsql("INSERT INTO orders (id, status) VALUES (1, 'open');\n").unwrap();
        assert_eq!(find_col(&p, "id", "write").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "status", "write").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn tsql_delete_where_is_read() {
        let p = parse_tsql("DELETE FROM orders WHERE status = 'closed';\n").unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn tsql_subquery_gets_its_own_scope_not_the_outer_ones() {
        // A naive "collect every field under the outer SELECT" would misattribute `total` (inside
        // the subquery, scoped to order_items) to orders' scope. The stop-at-nested-statement
        // boundary must prevent that.
        let p = parse_tsql(
            "SELECT status FROM orders WHERE id IN (SELECT order_id FROM order_items WHERE total > 100);\n",
        )
        .unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "order_id", "read").receiver_class.as_deref(), Some("order_items"));
        assert_eq!(find_col(&p, "total", "read").receiver_class.as_deref(), Some("order_items"));
    }

    #[test]
    fn tsql_sql_columns_off_never_reaches_dml_walk_via_default_gate() {
        // Belt-and-suspenders: the store-level gate (parse_one_file) is what actually turns this
        // off by default (tested in store.rs); this just confirms the walk itself has no separate
        // opt-out and always extracts when called directly, so the gate is the ONLY place this is
        // controlled — no risk of two disagreeing switches.
        let p = parse_tsql("SELECT status FROM orders;\n").unwrap();
        assert!(!p.calls.is_empty(), "walk layer always extracts; store.rs gates it");
    }

    // ---- L3.4: CTE lineage (tsql pilot) ----

    #[test]
    fn tsql_cte_projections_track_bare_passthrough_columns() {
        let p = parse_tsql(
            "WITH recent(order_id, cid) AS (\n  SELECT OrderID, CustomerID AS cid FROM Orders WHERE Total > 100\n)\nSELECT r.order_id, r.cid FROM recent r WHERE r.order_id > 0;\n",
        )
        .unwrap();
        let proj = p.cte_columns.get("recent").expect("recent CTE tracked");
        assert_eq!(proj.get("order_id"), Some(&("orders".to_string(), "orderid".to_string())));
        assert_eq!(proj.get("cid"), Some(&("orders".to_string(), "customerid".to_string())));
    }

    #[test]
    fn tsql_cte_projections_use_derived_name_without_explicit_column_list() {
        let p = parse_tsql("WITH old AS (SELECT OrderID FROM Orders)\nSELECT * FROM old;\n").unwrap();
        let proj = p.cte_columns.get("old").expect("old CTE tracked");
        assert_eq!(proj.get("orderid"), Some(&("orders".to_string(), "orderid".to_string())));
    }

    #[test]
    fn tsql_cte_projections_skip_computed_columns_never_guess() {
        // Mirrors the real dnn-clean-schema-dump/GetTabsByPackageID.sql shape: a window function
        // alongside bare passthroughs — only the passthroughs get an entry.
        let p = parse_tsql(
            "WITH Temp AS (\n  SELECT ROW_NUMBER() OVER (PARTITION BY TabId ORDER BY Version DESC) AS RowNumber, TabVersionId, TabId\n  FROM dbo.TabVersions WHERE IsPublished = 1\n)\nSELECT TabId FROM Temp;\n",
        )
        .unwrap();
        let proj = p.cte_columns.get("temp").expect("temp CTE tracked");
        assert!(!proj.contains_key("rownumber"), "window function output must never be guessed");
        assert_eq!(proj.get("tabversionid"), Some(&("tabversions".to_string(), "tabversionid".to_string())));
        assert_eq!(proj.get("tabid"), Some(&("tabversions".to_string(), "tabid".to_string())));
    }

    #[test]
    fn tsql_cte_projections_skip_ambiguous_join_body() {
        // The CTE body itself joins two tables with no qualifier on the output column — which
        // underlying table it came from is genuinely ambiguous, so no entry at all (never guess).
        let p = parse_tsql(
            "WITH j AS (SELECT id FROM orders o JOIN order_items i ON i.order_id = o.id)\nSELECT id FROM j;\n",
        )
        .unwrap();
        assert!(p.cte_columns.get("j").is_none_or(|m| !m.contains_key("id")));
    }

    // ---- PL/SQL ----

    #[test]
    fn plsql_create_table_extracts_table_and_column_defs() {
        // L3.2: CREATE TABLE used to produce zero defs (Wave L2 scope).
        let src = "CREATE TABLE orders (id NUMBER PRIMARY KEY, status VARCHAR2(20) NOT NULL);\n";
        let p = parse_plsql(src).unwrap();
        assert!(p.calls.is_empty());
        let table = p.defs.iter().find(|d| d.name == "orders" && d.kind == "table").expect("table def");
        assert!(table.parent_class.is_none());
        let id = p.defs.iter().find(|d| d.name == "id" && d.kind == "column").expect("id column");
        assert_eq!(id.parent_class.as_deref(), Some("orders"));
        let status = p.defs.iter().find(|d| d.name == "status" && d.kind == "column").expect("status column");
        assert_eq!(status.parent_class.as_deref(), Some("orders"));
        assert_eq!(p.defs.len(), 3, "table + 2 columns, nothing else");
    }

    // ---- L3.3: plsql DML column references ----

    #[test]
    fn plsql_select_qualified_and_unqualified_reads() {
        let p = parse_plsql(
            "BEGIN\n  SELECT o.status, total INTO v_s, v_t FROM orders o WHERE o.id = 5 AND total > 0;\nEND;\n/\n",
        )
        .unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        assert!(p
            .calls
            .iter()
            .filter(|c| c.name == "total" && c.kind == "read")
            .all(|c| c.receiver_class.as_deref() == Some("orders")));
    }

    #[test]
    fn plsql_join_widens_unqualified_scope() {
        let p = parse_plsql(
            "BEGIN\n  SELECT status INTO v_s FROM orders o JOIN order_items oi ON o.id = oi.order_id WHERE status = 'open';\nEND;\n/\n",
        )
        .unwrap();
        let scope = find_col(&p, "status", "read").receiver_class.clone().unwrap();
        let mut names: Vec<&str> = scope.split('\u{1f}').collect();
        names.sort();
        assert_eq!(names, vec!["order_items", "orders"]);
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "order_id", "read").receiver_class.as_deref(), Some("order_items"));
    }

    #[test]
    fn plsql_update_set_target_is_write_where_is_read() {
        let p = parse_plsql(
            "BEGIN\n  UPDATE orders SET status = 'closed', total = price WHERE id = 5;\nEND;\n/\n",
        )
        .unwrap();
        assert_eq!(find_col(&p, "status", "write").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "total", "write").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "price", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn plsql_insert_column_list_is_write() {
        let p = parse_plsql("BEGIN\n  INSERT INTO orders (id, status) VALUES (1, 'open');\nEND;\n/\n").unwrap();
        assert_eq!(find_col(&p, "id", "write").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "status", "write").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn plsql_delete_where_is_read() {
        let p = parse_plsql("BEGIN\n  DELETE FROM orders WHERE status = 'closed';\nEND;\n/\n").unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn plsql_subquery_gets_its_own_scope_not_the_outer_ones() {
        let p = parse_plsql(
            "BEGIN\n  SELECT status INTO v_s FROM orders WHERE id IN (SELECT order_id FROM order_items WHERE total > 100);\nEND;\n/\n",
        )
        .unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "order_id", "read").receiver_class.as_deref(), Some("order_items"));
        assert_eq!(find_col(&p, "total", "read").receiver_class.as_deref(), Some("order_items"));
    }

    #[test]
    fn plsql_function_call_in_select_is_not_a_column_read() {
        let p = parse_plsql(
            "BEGIN\n  SELECT ROUND(total, 2), NULLIF(status, 'x') FROM orders;\nEND;\n/\n",
        )
        .unwrap();
        assert!(p.calls.iter().all(|c| c.name != "round" && c.name != "nullif"));
        assert_eq!(find_col(&p, "total", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
    }

    // ---- L3.4: CTE lineage (plsql) ----

    #[test]
    fn plsql_cte_projections_track_bare_passthrough_columns() {
        let p = parse_plsql(
            "BEGIN\n  WITH recent(order_id, cid) AS (\n    SELECT order_id, customer_id FROM orders WHERE total > 100\n  )\n  SELECT r.order_id, r.cid INTO x, y FROM recent r WHERE r.order_id > 0;\nEND;\n/\n",
        )
        .unwrap();
        let proj = p.cte_columns.get("recent").expect("recent CTE tracked");
        assert_eq!(proj.get("order_id"), Some(&("orders".to_string(), "order_id".to_string())));
        assert_eq!(proj.get("cid"), Some(&("orders".to_string(), "customer_id".to_string())));
    }

    #[test]
    fn plsql_cte_projections_use_derived_name_without_explicit_column_list() {
        let p = parse_plsql(
            "BEGIN\n  WITH old AS (SELECT order_id FROM orders)\n  SELECT order_id INTO x FROM old;\nEND;\n/\n",
        )
        .unwrap();
        let proj = p.cte_columns.get("old").expect("old CTE tracked");
        assert_eq!(proj.get("order_id"), Some(&("orders".to_string(), "order_id".to_string())));
    }

    #[test]
    fn plsql_cte_projections_skip_computed_columns_never_guess() {
        let p = parse_plsql(
            "BEGIN\n  WITH counts AS (\n    SELECT ROUND(total, 2) AS rounded, order_id FROM order_items\n  )\n  SELECT order_id INTO x FROM counts;\nEND;\n/\n",
        )
        .unwrap();
        let proj = p.cte_columns.get("counts").expect("counts CTE tracked");
        assert!(!proj.contains_key("rounded"), "function-call output must never be guessed");
        assert_eq!(proj.get("order_id"), Some(&("order_items".to_string(), "order_id".to_string())));
    }

    #[test]
    fn plsql_cte_projections_skip_ambiguous_join_body() {
        let p = parse_plsql(
            "BEGIN\n  WITH j AS (\n    SELECT id FROM orders o JOIN order_items i ON i.order_id = o.id\n  )\n  SELECT id INTO x FROM j;\nEND;\n/\n",
        )
        .unwrap();
        assert!(p.cte_columns.get("j").is_none_or(|m| !m.contains_key("id")));
    }

    #[test]
    fn plsql_cte_body_own_reads_are_extracted() {
        // Unlike tsql/postgres, a plsql CTE body has no nested `statement`/`SelectStmt` wrapper for
        // the generic recursion to find on its own (see `plsql_walk_with_clause`'s doc comment) —
        // without its explicit push, the body's own WHERE-clause read was never extracted at all.
        let p = parse_plsql(
            "BEGIN\n  WITH recent AS (\n    SELECT order_id FROM orders WHERE total > 100\n  )\n  SELECT order_id INTO x FROM recent;\nEND;\n/\n",
        )
        .unwrap();
        assert_eq!(find_col(&p, "total", "read").receiver_class.as_deref(), Some("orders"));
    }

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
    fn pg_create_table_extracts_table_and_column_defs() {
        // L3.2: CREATE TABLE used to produce zero defs (Wave L2 scope).
        let p = parse_postgres("CREATE TABLE t (id int primary key, status varchar(20));\n").unwrap();
        assert!(p.calls.is_empty());
        let table = p.defs.iter().find(|d| d.name == "t" && d.kind == "table").expect("table def");
        assert!(table.parent_class.is_none());
        let id = p.defs.iter().find(|d| d.name == "id" && d.kind == "column").expect("id column");
        assert_eq!(id.parent_class.as_deref(), Some("t"));
        let status = p.defs.iter().find(|d| d.name == "status" && d.kind == "column").expect("status column");
        assert_eq!(status.parent_class.as_deref(), Some("t"));
        assert_eq!(p.defs.len(), 3, "table + 2 columns, nothing else");
    }

    #[test]
    fn pg_schema_qualified_table_extracts_the_table_not_the_schema() {
        // Regression: `qualified_name`'s object name lives in `indirection`'s `attr_name` when
        // schema-qualified, NOT in the bare `ColId` (which holds the schema part instead) — found
        // dogfooding the real Wide World Importers schema, where every table is schema-qualified
        // and every one of them was extracting as the wrong (schema) name before this fix.
        let p = parse_postgres("CREATE TABLE Sales.BuyingGroups (BuyingGroupID int, Name varchar(50));\n").unwrap();
        let table = p.defs.iter().find(|d| d.kind == "table").expect("table def");
        assert_eq!(table.name, "buyinggroups", "not 'sales' (the schema)");
        let id = p.defs.iter().find(|d| d.name == "buyinggroupid").expect("id column");
        assert_eq!(id.parent_class.as_deref(), Some("buyinggroups"));
        // Regression: a column whose name is also a SQL keyword-ish word (`Name`) tags its ColId's
        // inner leaf as `unreserved_keyword` instead of `identifier` — reaching for `identifier`
        // specifically (as this code used to) silently drops the column. Taking ColId's own span
        // works for both leaf shapes.
        assert!(p.defs.iter().any(|d| d.name == "name" && d.kind == "column"), "{:?}", p.defs);
    }

    #[test]
    fn pg_schema_qualified_insert_target_resolves_the_table() {
        let p = parse_postgres("INSERT INTO Sales.BuyingGroups (BuyingGroupID) VALUES (1);\n").unwrap();
        assert_eq!(
            find_col(&p, "buyinggroupid", "write").receiver_class.as_deref(),
            Some("buyinggroups")
        );
    }

    // ---- L3.3: postgres DML column references ----

    #[test]
    fn pg_select_qualified_and_unqualified_reads() {
        let p = parse_postgres(
            "SELECT o.status, total FROM orders o WHERE o.id = 5 AND total > 0;\n",
        )
        .unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        assert!(p
            .calls
            .iter()
            .filter(|c| c.name == "total" && c.kind == "read")
            .all(|c| c.receiver_class.as_deref() == Some("orders")));
    }

    #[test]
    fn pg_join_widens_unqualified_scope() {
        let p = parse_postgres(
            "SELECT status FROM orders o JOIN order_items oi ON o.id = oi.order_id WHERE status = 'open';\n",
        )
        .unwrap();
        let scope = find_col(&p, "status", "read").receiver_class.clone().unwrap();
        let mut names: Vec<&str> = scope.split('\u{1f}').collect();
        names.sort();
        assert_eq!(names, vec!["order_items", "orders"]);
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "order_id", "read").receiver_class.as_deref(), Some("order_items"));
    }

    #[test]
    fn pg_update_set_target_is_write_where_is_read() {
        let p = parse_postgres("UPDATE orders SET status = 'closed', total = price WHERE id = 5;\n").unwrap();
        assert_eq!(find_col(&p, "status", "write").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "total", "write").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "price", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn pg_insert_column_list_is_write() {
        let p = parse_postgres("INSERT INTO orders (id, status) VALUES (1, 'open');\n").unwrap();
        assert_eq!(find_col(&p, "id", "write").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "status", "write").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn pg_delete_where_is_read() {
        let p = parse_postgres("DELETE FROM orders WHERE status = 'closed';\n").unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
    }

    #[test]
    fn pg_subquery_gets_its_own_scope_not_the_outer_ones() {
        let p = parse_postgres(
            "SELECT status FROM orders WHERE id IN (SELECT order_id FROM order_items WHERE total > 100);\n",
        )
        .unwrap();
        assert_eq!(find_col(&p, "status", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "id", "read").receiver_class.as_deref(), Some("orders"));
        assert_eq!(find_col(&p, "order_id", "read").receiver_class.as_deref(), Some("order_items"));
        assert_eq!(find_col(&p, "total", "read").receiver_class.as_deref(), Some("order_items"));
    }

    // ---- L3.4: CTE lineage (postgres) ----

    #[test]
    fn pg_cte_projections_track_bare_passthrough_columns() {
        let p = parse_postgres(
            "WITH recent(order_id, cid) AS (\n  SELECT orderid, customerid AS cid FROM orders WHERE total > 100\n)\nSELECT r.order_id, r.cid FROM recent r WHERE r.order_id > 0;\n",
        )
        .unwrap();
        let proj = p.cte_columns.get("recent").expect("recent CTE tracked");
        assert_eq!(proj.get("order_id"), Some(&("orders".to_string(), "orderid".to_string())));
        assert_eq!(proj.get("cid"), Some(&("orders".to_string(), "customerid".to_string())));
    }

    #[test]
    fn pg_cte_projections_use_derived_name_without_explicit_column_list() {
        let p = parse_postgres("WITH old AS (SELECT orderid FROM orders)\nSELECT * FROM old;\n").unwrap();
        let proj = p.cte_columns.get("old").expect("old CTE tracked");
        assert_eq!(proj.get("orderid"), Some(&("orders".to_string(), "orderid".to_string())));
    }

    #[test]
    fn pg_cte_projections_skip_computed_columns_never_guess() {
        let p = parse_postgres(
            "WITH counts AS (\n  SELECT count(*) AS total, order_id FROM order_items GROUP BY order_id\n)\nSELECT order_id FROM counts;\n",
        )
        .unwrap();
        let proj = p.cte_columns.get("counts").expect("counts CTE tracked");
        assert!(!proj.contains_key("total"), "aggregate output must never be guessed");
        assert_eq!(proj.get("order_id"), Some(&("order_items".to_string(), "order_id".to_string())));
    }

    #[test]
    fn pg_cte_projections_skip_ambiguous_join_body() {
        let p = parse_postgres(
            "WITH j AS (SELECT id FROM orders o JOIN order_items i ON i.order_id = o.id)\nSELECT id FROM j;\n",
        )
        .unwrap();
        assert!(p.cte_columns.get("j").is_none_or(|m| !m.contains_key("id")));
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
