//! L1.3 — universal-tier walks for the 8 non-Python languages (Rust, C, C++, C#, Java,
//! JavaScript, TypeScript/TSX, Go). Each walk is deliberately shallow and honest: defs with their
//! parent container, calls split func-vs-method by syntax alone, imports/aliases where the language
//! has them, and receiver-class hints ONLY where syntactically free (Go method receivers, Rust
//! `self` inside an `impl T`) — never inferred. Everything else stays None and resolves through the
//! universal name-based path in `store::resolve_call` (lang-scoped, L1.2).

use crate::parser::{first_line, text, Alias, CallSite, Definition, Import, ImportName, ParsedFile};
use tree_sitter::{Language, Node, Parser, Tree};

fn tree_for(src: &str, language: Language, what: &str) -> anyhow::Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language).map_err(|e| anyhow::anyhow!("load {what} grammar: {e}"))?;
    parser.parse(src, None).ok_or_else(|| anyhow::anyhow!("tree-sitter returned no tree"))
}

fn mk_def(
    name: &str,
    kind: &str,
    parent: Option<&str>,
    node: Node,
    src: &[u8],
    docstring: Option<String>,
) -> Definition {
    Definition {
        name: name.to_string(),
        kind: kind.into(),
        parent_class: parent.map(str::to_string),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        signature: first_line(node, src),
        ret_class: None,  // universal tier: no return-type narrowing (Python-only, S2/T1)
        base_class: None, // universal tier: no inheritance hop (Python-only, T4)
        docstring,
    }
}

fn push_call(out: &mut ParsedFile, name: &str, kind: &str, node: Node, enclosing: &str, receiver_class: Option<String>) {
    if name.is_empty() {
        return;
    }
    out.calls.push(CallSite {
        name: name.to_string(),
        kind: kind.into(),
        line: node.start_position().row + 1,
        enclosing: enclosing.to_string(),
        receiver_class,
    });
}

fn push_import(out: &mut ParsedFile, node: Node, src: &[u8]) {
    out.imports.push(Import { raw: first_line(node, src), line: node.start_position().row + 1 });
}

fn is_comment(kind: &str) -> bool {
    matches!(kind, "comment" | "line_comment" | "block_comment")
}

/// Docstring "where it's cheap" (spec L1.3): the doc comment directly above a def — Rust `///`
/// runs (walked back to the run's first line) and `/** */` / `///` blocks for Java/C#/JS/TS.
/// Anything that isn't doc-marked (plain `//`, `/*`), or separated from the def, -> None.
fn leading_doc(node: Node, src: &[u8]) -> Option<String> {
    let mut first = node.prev_named_sibling()?;
    if !is_comment(first.kind()) {
        return None;
    }
    // a `///` run is one node per line — walk back to the run's first line (adjacent lines only)
    while let Some(prev) = first.prev_named_sibling() {
        if is_comment(prev.kind()) && prev.end_position().row + 1 >= first.start_position().row {
            first = prev;
        } else {
            break;
        }
    }
    doc_first_line(text(first, src))
}

/// First content line of a doc comment (`///` or `/** */` style), ≤120 chars; plain comments -> None.
fn doc_first_line(t: &str) -> Option<String> {
    if !(t.starts_with("///") || t.starts_with("/**")) {
        return None;
    }
    for line in t.lines() {
        let l = line
            .trim()
            .trim_start_matches('/')
            .trim_start_matches('*')
            .trim_end_matches("*/")
            .trim();
        if !l.is_empty() {
            return Some(l.chars().take(120).collect());
        }
    }
    None
}

fn last_segment(path: &str, sep: &str) -> String {
    path.rsplit(sep).next().unwrap_or(path).trim().to_string()
}

fn walk_children<F: FnMut(Node)>(node: Node, mut f: F) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        f(child);
    }
}

fn find_child<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    let mut found = None;
    for c in node.named_children(&mut cursor) {
        if c.kind() == kind {
            found = Some(c);
            break;
        }
    }
    found
}

// ---- Rust -------------------------------------------------------------------

pub fn parse_rust(src: &str) -> anyhow::Result<ParsedFile> {
    let tree = tree_for(src, tree_sitter_rust::LANGUAGE.into(), "rust")?;
    let mut out = ParsedFile::default();
    walk_rust(tree.root_node(), src.as_bytes(), &mut out, RustCtx { enclosing: "<module>", container: None, self_class: None });
    Ok(out)
}

#[derive(Clone, Copy)]
struct RustCtx<'a> {
    enclosing: &'a str,
    container: Option<&'a str>,  // impl-type / trait name for direct children
    self_class: Option<&'a str>, // what `self` refers to inside the current fn (closures keep it)
}

fn walk_rust<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: RustCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "function_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = RustCtx {
                    enclosing: nm,
                    container: None, // fns nested inside this one are not methods
                    self_class: ctx.container.or(ctx.self_class),
                };
            }
        }
        "struct_item" | "enum_item" | "trait_item" | "union_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, leading_doc(node, src)));
                if node.kind() == "trait_item" {
                    child_ctx = RustCtx { container: Some(nm), ..ctx }; // default methods
                }
            }
        }
        "impl_item" => {
            // `impl T` / `impl Trait for T` — methods' parent is T. Only a plain (or generic-base)
            // type name binds; anything fancier leaves container None (honest).
            let tname = node.child_by_field_name("type").and_then(|t| match t.kind() {
                "type_identifier" => Some(text(t, src)),
                "generic_type" => t
                    .child_by_field_name("type")
                    .filter(|b| b.kind() == "type_identifier")
                    .map(|b| text(b, src)),
                _ => None,
            });
            if let Some(tn) = tname {
                child_ctx = RustCtx { container: Some(tn), ..ctx };
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, ctx.enclosing, None),
                    "field_expression" => {
                        if let Some(field) = f.child_by_field_name("field") {
                            // hint only for the syntactically-free case: `self.foo()` in `impl T`
                            let recv = f
                                .child_by_field_name("value")
                                .filter(|v| v.kind() == "self")
                                .and_then(|_| ctx.self_class.map(str::to_string));
                            push_call(out, text(field, src), "method", node, ctx.enclosing, recv);
                        }
                    }
                    // `X::y(...)` — resolve by the member name only; no receiver hint (spec: hints
                    // are Go receivers + Rust `self` only, everything else None).
                    "scoped_identifier" => {
                        if let Some(name) = f.child_by_field_name("name") {
                            push_call(out, text(name, src), "method", node, ctx.enclosing, None);
                        }
                    }
                    _ => {}
                }
            }
        }
        "use_declaration" => push_import(out, node, src),
        "use_as_clause" => {
            // `use a::b as c` (also inside use-lists) — alias feeds the shared expansion path
            if let (Some(path), Some(alias)) = (node.child_by_field_name("path"), node.child_by_field_name("alias")) {
                out.aliases.push(Alias { local: text(alias, src).to_string(), source: last_segment(text(path, src), "::") });
            }
        }
        // `mod foo;` (no body) is import-ish — recording it keeps decl-only files from being
        // flagged "suspect" (zero defs/calls/imports). `mod foo { .. }` just descends.
        "mod_item" if node.child_by_field_name("body").is_none() => push_import(out, node, src),
        _ => {}
    }
    walk_children(node, |c| walk_rust(c, src, out, child_ctx));
}

// ---- Go ---------------------------------------------------------------------

pub fn parse_go(src: &str) -> anyhow::Result<ParsedFile> {
    let tree = tree_for(src, tree_sitter_go::LANGUAGE.into(), "go")?;
    let mut out = ParsedFile::default();
    walk_go(tree.root_node(), src.as_bytes(), &mut out, GoCtx { enclosing: "<module>", recv_var: None, recv_type: None });
    Ok(out)
}

#[derive(Clone, Copy)]
struct GoCtx<'a> {
    enclosing: &'a str,
    recv_var: Option<&'a str>,  // the method receiver's identifier (e.g. `w`)
    recv_type: Option<&'a str>, // its type name (e.g. `Widget`)
}

fn walk_go<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: GoCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "function_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", None, node, src, None));
                child_ctx = GoCtx { enclosing: nm, ..ctx };
            }
        }
        "method_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                let mut recv_var = None;
                let mut recv_type = None;
                if let Some(recv) = node.child_by_field_name("receiver") {
                    if let Some(pd) = find_child(recv, "parameter_declaration") {
                        recv_var = pd
                            .child_by_field_name("name")
                            .filter(|n| n.kind() == "identifier")
                            .map(|n| text(n, src));
                        recv_type = pd.child_by_field_name("type").and_then(|t| match t.kind() {
                            "type_identifier" => Some(text(t, src)),
                            "pointer_type" => {
                                t.named_child(0).filter(|i| i.kind() == "type_identifier").map(|i| text(i, src))
                            }
                            _ => None,
                        });
                    }
                }
                out.defs.push(mk_def(nm, "function", recv_type, node, src, None));
                child_ctx = GoCtx { enclosing: nm, recv_var, recv_type };
            }
        }
        // any named type (struct/interface/alias) — they can all carry methods
        "type_spec" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.defs.push(mk_def(text(name, src), "class", None, node, src, None));
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, ctx.enclosing, None),
                    "selector_expression" => {
                        if let Some(field) = f.child_by_field_name("field") {
                            // free hint: `w.foo()` where `w` is this method's own receiver ident
                            let recv = f
                                .child_by_field_name("operand")
                                .filter(|o| o.kind() == "identifier" && Some(text(*o, src)) == ctx.recv_var)
                                .and_then(|_| ctx.recv_type.map(str::to_string));
                            push_call(out, text(field, src), "method", node, ctx.enclosing, recv);
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_spec" => {
            push_import(out, node, src);
            // `import f "fmt"` — alias f -> last path segment of the package path
            if let (Some(name), Some(path)) = (node.child_by_field_name("name"), node.child_by_field_name("path")) {
                let pkg = text(path, src).trim_matches('"').to_string();
                out.aliases.push(Alias { local: text(name, src).to_string(), source: last_segment(&pkg, "/") });
            }
        }
        _ => {}
    }
    walk_children(node, |c| walk_go(c, src, out, child_ctx));
}

// ---- C ----------------------------------------------------------------------

pub fn parse_c(src: &str) -> anyhow::Result<ParsedFile> {
    let tree = tree_for(src, tree_sitter_c::LANGUAGE.into(), "c")?;
    let mut out = ParsedFile::default();
    walk_c(tree.root_node(), src.as_bytes(), &mut out, "<module>");
    Ok(out)
}

/// C/C++ — descend a `function_definition`'s declarator chain (pointer/reference wrappers) to the
/// `function_declarator`'s own declarator node (the name). None for shapes we don't handle
/// (function pointers, destructors, operators) — those defs are skipped, honestly.
fn c_declarator_name(node: Node) -> Option<Node> {
    let mut d = node.child_by_field_name("declarator")?;
    loop {
        match d.kind() {
            "pointer_declarator" | "reference_declarator" => d = d.child_by_field_name("declarator")?,
            "function_declarator" => {
                let inner = d.child_by_field_name("declarator")?;
                return matches!(inner.kind(), "identifier" | "field_identifier" | "qualified_identifier")
                    .then_some(inner);
            }
            _ => return None,
        }
    }
}

fn walk_c<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, enclosing: &'a str) {
    let mut child_enclosing = enclosing;
    match node.kind() {
        "function_definition" => {
            if let Some(name) = c_declarator_name(node) {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", None, node, src, None));
                child_enclosing = nm;
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, enclosing, None),
                    // C has no methods: `s->fn()` / `s.fn()` stays kind `func` (spec L1.3 note)
                    "field_expression" => {
                        if let Some(field) = f.child_by_field_name("field") {
                            push_call(out, text(field, src), "func", node, enclosing, None);
                        }
                    }
                    _ => {}
                }
            }
        }
        "preproc_include" => push_import(out, node, src),
        _ => {}
    }
    walk_children(node, |c| walk_c(c, src, out, child_enclosing));
}

// ---- C++ --------------------------------------------------------------------

pub fn parse_cpp(src: &str) -> anyhow::Result<ParsedFile> {
    let tree = tree_for(src, tree_sitter_cpp::LANGUAGE.into(), "cpp")?;
    let mut out = ParsedFile::default();
    walk_cpp(tree.root_node(), src.as_bytes(), &mut out, CppCtx { enclosing: "<module>", container: None });
    Ok(out)
}

#[derive(Clone, Copy)]
struct CppCtx<'a> {
    enclosing: &'a str,
    container: Option<&'a str>, // enclosing class/struct body
}

fn walk_cpp<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: CppCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "class_specifier" | "struct_specifier" => {
            // only a definition (name + body) is a def; bare `struct X` type references are not
            if let (Some(name), Some(_body)) = (node.child_by_field_name("name"), node.child_by_field_name("body")) {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, None));
                child_ctx = CppCtx { container: Some(nm), ..ctx };
            }
        }
        "function_definition" => {
            if let Some(name) = c_declarator_name(node) {
                let (nm, parent) = match name.kind() {
                    // out-of-line `Widget::extra() { .. }` — cheap qualified definition (spec L1.3)
                    "qualified_identifier" => {
                        let n = name.child_by_field_name("name").filter(|n| n.kind() == "identifier");
                        let scope = name
                            .child_by_field_name("scope")
                            .filter(|s| s.kind() == "namespace_identifier")
                            .map(|s| text(s, src));
                        match n {
                            Some(n) => (text(n, src), scope),
                            None => ("", None), // templated/destructor names — skip, stay shallow
                        }
                    }
                    _ => (text(name, src), ctx.container),
                };
                if !nm.is_empty() {
                    out.defs.push(mk_def(nm, "function", parent, node, src, None));
                    child_ctx = CppCtx { enclosing: nm, container: None };
                }
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, ctx.enclosing, None),
                    "field_expression" => {
                        if let Some(field) = f.child_by_field_name("field") {
                            push_call(out, text(field, src), "method", node, ctx.enclosing, None);
                        }
                    }
                    // `X::y()` — member name only, no hint; C++ is allowed to over-report
                    // unresolved rather than grow clever (spec L1.3 note)
                    "qualified_identifier" => {
                        if let Some(n) = f.child_by_field_name("name").filter(|n| n.kind() == "identifier") {
                            push_call(out, text(n, src), "method", node, ctx.enclosing, None);
                        }
                    }
                    _ => {}
                }
            }
        }
        "preproc_include" => push_import(out, node, src),
        _ => {}
    }
    walk_children(node, |c| walk_cpp(c, src, out, child_ctx));
}

// ---- C# ---------------------------------------------------------------------

pub fn parse_csharp(src: &str) -> anyhow::Result<ParsedFile> {
    let tree = tree_for(src, tree_sitter_c_sharp::LANGUAGE.into(), "c-sharp")?;
    let mut out = ParsedFile::default();
    walk_csharp(tree.root_node(), src.as_bytes(), &mut out, CppCtx { enclosing: "<module>", container: None });
    Ok(out)
}

fn walk_csharp<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: CppCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "class_declaration" | "interface_declaration" | "struct_declaration" | "record_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { container: Some(nm), ..ctx };
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { enclosing: nm, container: None };
            }
        }
        "local_function_statement" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", None, node, src, None));
                child_ctx = CppCtx { enclosing: nm, container: None };
            }
        }
        "invocation_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, ctx.enclosing, None),
                    "member_access_expression" => {
                        if let Some(name) = f.child_by_field_name("name") {
                            push_call(out, text(name, src), "method", node, ctx.enclosing, None);
                        }
                    }
                    _ => {}
                }
            }
        }
        "using_directive" => {
            push_import(out, node, src);
            // `using Alias = Some.Type;` — alias only when the source's rightmost name is a plain
            // identifier (generic aliases like List<int> are skipped, honestly)
            if let Some(alias) = node.child_by_field_name("name") {
                let value = node.named_child(node.named_child_count().saturating_sub(1));
                let source = value.and_then(|v| match v.kind() {
                    "identifier" => Some(text(v, src).to_string()),
                    "qualified_name" => v
                        .child_by_field_name("name")
                        .filter(|n| n.kind() == "identifier")
                        .map(|n| text(n, src).to_string()),
                    _ => None,
                });
                if let Some(source) = source {
                    out.aliases.push(Alias { local: text(alias, src).to_string(), source });
                }
            }
        }
        _ => {}
    }
    walk_children(node, |c| walk_csharp(c, src, out, child_ctx));
}

// ---- Java -------------------------------------------------------------------

pub fn parse_java(src: &str) -> anyhow::Result<ParsedFile> {
    let tree = tree_for(src, tree_sitter_java::LANGUAGE.into(), "java")?;
    let mut out = ParsedFile::default();
    walk_java(tree.root_node(), src.as_bytes(), &mut out, CppCtx { enclosing: "<module>", container: None });
    Ok(out)
}

fn walk_java<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: CppCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { container: Some(nm), ..ctx };
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { enclosing: nm, container: None };
            }
        }
        "method_invocation" => {
            if let Some(name) = node.child_by_field_name("name") {
                // `x.foo()` -> method; bare `foo()` -> func (universal split, no hints)
                let kind = if node.child_by_field_name("object").is_some() { "method" } else { "func" };
                push_call(out, text(name, src), kind, node, ctx.enclosing, None);
            }
        }
        "import_declaration" => {
            push_import(out, node, src);
            // `import a.b.C;` -> local C from a.b (last segment, spec L1.3); wildcard imports skip
            let mut wc = node.walk();
            let wildcard = node.children(&mut wc).any(|c| c.kind() == "asterisk");
            if !wildcard {
                if let Some(scoped) = find_child(node, "scoped_identifier") {
                    let full = text(scoped, src);
                    if let Some((prefix, local)) = full.rsplit_once('.') {
                        out.import_names
                            .push(ImportName { local: local.to_string(), source_module: prefix.to_string() });
                    }
                }
            }
        }
        _ => {}
    }
    walk_children(node, |c| walk_java(c, src, out, child_ctx));
}

// ---- JavaScript / TypeScript / TSX (one walk, three grammars) -----------------

pub fn parse_javascript(src: &str) -> anyhow::Result<ParsedFile> {
    parse_js_family(src, tree_sitter_javascript::LANGUAGE.into(), "javascript")
}

pub fn parse_typescript(src: &str) -> anyhow::Result<ParsedFile> {
    parse_js_family(src, tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), "typescript")
}

pub fn parse_tsx(src: &str) -> anyhow::Result<ParsedFile> {
    parse_js_family(src, tree_sitter_typescript::LANGUAGE_TSX.into(), "tsx")
}

fn parse_js_family(src: &str, language: Language, what: &str) -> anyhow::Result<ParsedFile> {
    let tree = tree_for(src, language, what)?;
    let mut out = ParsedFile::default();
    walk_js(tree.root_node(), src.as_bytes(), &mut out, CppCtx { enclosing: "<module>", container: None });
    Ok(out)
}

fn walk_js<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: CppCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", None, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { enclosing: nm, container: None };
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { container: Some(nm), ..ctx };
            }
        }
        "method_definition" => {
            if let Some(name) = node.child_by_field_name("name").filter(|n| n.kind() == "property_identifier") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { enclosing: nm, container: None };
            }
        }
        // `const x = () => ..` / `const x = function ..` — cheap and very common (spec L1.3)
        "variable_declarator" => {
            if let (Some(name), Some(value)) = (node.child_by_field_name("name"), node.child_by_field_name("value")) {
                if name.kind() == "identifier"
                    && matches!(value.kind(), "arrow_function" | "function_expression" | "function")
                {
                    let nm = text(name, src);
                    out.defs.push(mk_def(nm, "function", None, node, src, None));
                    child_ctx = CppCtx { enclosing: nm, container: None };
                }
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, ctx.enclosing, None),
                    "member_expression" => {
                        if let Some(prop) = f.child_by_field_name("property").filter(|p| p.kind() == "property_identifier") {
                            push_call(out, text(prop, src), "method", node, ctx.enclosing, None);
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_statement" => {
            push_import(out, node, src);
            if let Some(source) = node.child_by_field_name("source") {
                let module = text(source, src).trim_matches(|c| c == '"' || c == '\'').to_string();
                collect_js_import_names(node, src, &module, out);
            }
        }
        _ => {}
    }
    walk_children(node, |c| walk_js(c, src, out, child_ctx));
}

/// named/default import bindings: `import { a as b, c } from "m"` -> alias b->a + locals b,c from m;
/// `import Def from "m"` -> local Def from m. Namespace imports (`* as ns`) are skipped — `ns.f()`
/// is a method-kind call, out of alias scope.
fn collect_js_import_names(node: Node, src: &[u8], module: &str, out: &mut ParsedFile) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "import_specifier" => {
                let name = n.child_by_field_name("name").map(|x| text(x, src).to_string());
                let alias = n.child_by_field_name("alias").map(|x| text(x, src).to_string());
                if let Some(name) = name {
                    if let Some(alias) = alias {
                        out.import_names
                            .push(ImportName { local: alias.clone(), source_module: module.to_string() });
                        out.aliases.push(Alias { local: alias, source: name });
                    } else {
                        out.import_names.push(ImportName { local: name, source_module: module.to_string() });
                    }
                }
                continue;
            }
            "import_clause" => {
                // default import: a bare identifier directly under the clause
                let mut c = n.walk();
                for child in n.named_children(&mut c) {
                    if child.kind() == "identifier" {
                        out.import_names
                            .push(ImportName { local: text(child, src).to_string(), source_module: module.to_string() });
                    }
                }
            }
            _ => {}
        }
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
    }
}
