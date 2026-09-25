//! L1.3 — universal-tier walks for the 8 non-Python languages (Rust, C, C++, C#, Java,
//! JavaScript, TypeScript/TSX, Go). Each walk is deliberately shallow and honest: defs with their
//! parent container, calls split func-vs-method by syntax alone, imports/aliases where the language
//! has them, and receiver-class hints ONLY where syntactically free (Go method receivers, Rust
//! `self` inside an `impl T`) — never inferred. Everything else stays None and resolves through the
//! universal name-based path in `store::resolve_call` (lang-scoped, L1.2).

use crate::parser::{first_line, text, Alias, CallSite, Definition, Import, ImportName, ParsedFile};
use tree_sitter::{Language, Node, Parser, Tree};

pub(crate) fn tree_for(src: &str, language: Language, what: &str) -> anyhow::Result<Tree> {
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

pub(crate) fn walk_children<F: FnMut(Node)>(node: Node, mut f: F) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        f(child);
    }
}

/// A class's FIRST base type as a plain name (last segment of a qualified/generic/member form):
/// `extends a.B<T>` -> B, `: public ns::Base` -> Base. Recorded as `base_class` so a `this.m()`
/// the class doesn't define itself resolves one hop up (T4). Anything fancier -> None.
fn first_base_name(class_node: Node, src: &[u8]) -> Option<String> {
    fn type_name(n: Node, src: &[u8]) -> Option<String> {
        match n.kind() {
            "identifier" | "type_identifier" => Some(text(n, src).to_string()),
            "member_expression" => n.child_by_field_name("property").map(|p| text(p, src).to_string()),
            "qualified_identifier" | "qualified_name" | "scoped_type_identifier" | "generic_name" | "template_type"
            | "generic_type" => n
                .child_by_field_name("name")
                .or_else(|| {
                    let mut c = n.walk();
                    let kids: Vec<Node> = n.named_children(&mut c).collect();
                    // scoped: last segment; generic: the base name comes first
                    if n.kind().starts_with("generic") { kids.first().copied() } else { kids.last().copied() }
                })
                .and_then(|x| type_name(x, src)),
            _ => None,
        }
    }
    let base = if let Some(sc) = class_node.child_by_field_name("superclass") {
        sc.named_child(0) // java: `extends T`
    } else if let Some(h) = find_child(class_node, "class_heritage") {
        match find_child(h, "extends_clause") {
            Some(e) => e.child_by_field_name("value"), // ts
            None => h.named_child(0),                  // js
        }
    } else if let Some(b) = find_child(class_node, "base_list").or_else(|| find_child(class_node, "base_class_clause")) {
        let mut c = b.walk();
        let first = b.named_children(&mut c).find(|k| !matches!(k.kind(), "access_specifier" | "attribute_declaration" | "argument_list"));
        first // c# / c++
    } else {
        None
    };
    base.and_then(|b| type_name(b, src))
}

/// `ident:X` receivers -> the one type every binding of X in the call's function agrees on (else
/// no hint: two types for one name, or no binding at all, is never guessed). Keyed by enclosing
/// function NAME, like Python's T1, so same-named functions pool their bindings (conservative).
/// Never leaves an `ident:` marker behind.
fn bind_local_receivers(out: &mut ParsedFile) {
    let mut types: std::collections::HashMap<(String, String), Option<String>> = std::collections::HashMap::new();
    for (f, v, t) in std::mem::take(&mut out.var_bindings) {
        types
            .entry((f, v))
            .and_modify(|cur| {
                if cur.as_deref() != Some(t.as_str()) {
                    *cur = None;
                }
            })
            .or_insert(Some(t));
    }
    for c in &mut out.calls {
        if let Some(x) = c.receiver_class.as_deref().and_then(|r| r.strip_prefix("ident:")) {
            // "?" = assigned something of unknown type (JS reassignment): never a hint
            c.receiver_class =
                types.get(&(c.enclosing.clone(), x.to_string())).cloned().flatten().filter(|t| t != "?");
        }
    }
}

/// A Java/C#/C++ declared type as a plain class name: `T`, `a.T`, `ns::T` (and `T*` / `T&` via the
/// declarator, see `cfam_decl_name`). Generic containers (`List<T>`, `std::vector<T>`), `var` /
/// `auto`, and primitives -> None.
fn cfam_type_name<'a>(t: Node, src: &'a [u8]) -> Option<&'a str> {
    match t.kind() {
        "type_identifier" | "identifier" => Some(text(t, src)).filter(|n| *n != "var"),
        "scoped_type_identifier" | "qualified_name" | "qualified_identifier" => {
            t.child_by_field_name("name").and_then(|n| cfam_type_name(n, src))
        }
        _ => None,
    }
}

/// The variable a Java/C#/C++ declarator names, seeing through `*`, `&` and `= init`; plus the
/// initializer when there is one.
fn cfam_decl_name<'a>(d: Node<'a>, src: &'a [u8]) -> Option<(&'a str, Option<Node<'a>>)> {
    match d.kind() {
        "identifier" => Some((text(d, src), None)),
        "variable_declarator" | "init_declarator" => {
            let inner = d.child_by_field_name("name").or_else(|| d.child_by_field_name("declarator"))?;
            let value = d.child_by_field_name("value").or_else(|| {
                // c#: the initializer is a bare expression child after the name
                let mut c = d.walk();
                let v = d.named_children(&mut c).find(|k| k.id() != inner.id() && k.kind() != "bracketed_argument_list");
                v
            });
            cfam_decl_name(inner, src).map(|(n, _)| (n, value))
        }
        "pointer_declarator" => d.child_by_field_name("declarator").and_then(|i| cfam_decl_name(i, src)),
        "reference_declarator" => d.named_child(0).and_then(|i| cfam_decl_name(i, src)),
        _ => None,
    }
}

/// `new T(..)` in Java/C#/C++ -> T (what a `var` / `auto` local holds).
fn cfam_new_type<'a>(v: Node, src: &'a [u8]) -> Option<&'a str> {
    matches!(v.kind(), "object_creation_expression" | "new_expression")
        .then(|| v.child_by_field_name("type"))
        .flatten()
        .and_then(|t| cfam_type_name(t, src))
}

/// A local/param declaration's bindings: explicit class type, else (`var` / `auto`) the `new T`
/// initializer's type.
fn push_cfam_bindings<'a>(out: &mut ParsedFile, enclosing: &str, ty: Option<Node>, decls: &[Node<'a>], src: &'a [u8]) {
    let declared = ty.and_then(|t| cfam_type_name(t, src));
    for d in decls {
        if let Some((name, value)) = cfam_decl_name(*d, src) {
            if let Some(t) = declared.or_else(|| value.and_then(|v| cfam_new_type(v, src))) {
                out.var_bindings.push((enclosing.to_string(), name.to_string(), t.to_string()));
            }
        }
    }
}

/// `x.m()` on a plain identifier -> `ident:x` for `bind_local_receivers`.
fn ident_hint(obj: Option<Node>, src: &[u8]) -> Option<String> {
    obj.filter(|o| o.kind() == "identifier").map(|o| format!("ident:{}", text(o, src)))
}

/// `this.m()` / `this->m()`: the receiver object is `this` -> the enclosing class, if known.
fn this_hint(obj: Option<Node>, ctx: CppCtx) -> Option<String> {
    obj.filter(|o| o.kind() == "this").and(ctx.this_class).map(str::to_string)
}

pub(crate) fn find_child<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
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
    bind_local_receivers(&mut out);
    Ok(out)
}

/// A Rust type as a plain struct name: `T`, `&T`, `&mut T`, `a::T`. Generic wrappers (`Vec<T>`,
/// `Box<T>`, `Option<T>`) -> None: the receiver isn't a T.
fn rust_type_name<'a>(t: Node, src: &'a [u8]) -> Option<&'a str> {
    match t.kind() {
        "type_identifier" => Some(text(t, src)),
        "reference_type" => t.child_by_field_name("type").and_then(|i| rust_type_name(i, src)),
        "scoped_type_identifier" => t.child_by_field_name("name").map(|n| text(n, src)),
        _ => None,
    }
}

/// The type a `let` initializer evidently constructs: `T { .. }`, or a constructor-named associated
/// call `T::new*` / `T::with_*` / `T::from*` / `T::default()` (`Self::` -> the impl type), seen
/// through a trailing `?` / `.unwrap()` / `.expect(..)`. Other associated fns (`T::open()` may
/// return `Result<T>`, `T::builder()` another type) -> None.
fn rust_ctor_type<'a>(v: Node, src: &'a [u8], self_class: Option<&'a str>) -> Option<&'a str> {
    match v.kind() {
        "try_expression" => v.named_child(0).and_then(|i| rust_ctor_type(i, src, self_class)),
        "struct_expression" => v.child_by_field_name("name").and_then(|n| match n.kind() {
            "type_identifier" => Some(text(n, src)),
            "scoped_type_identifier" => n.child_by_field_name("name").map(|x| text(x, src)),
            _ => None,
        }),
        "call_expression" => {
            let f = v.child_by_field_name("function")?;
            match f.kind() {
                // `.unwrap()` / `.expect(..)` on a constructor call
                "field_expression" => f
                    .child_by_field_name("field")
                    .filter(|m| matches!(text(*m, src), "unwrap" | "expect"))
                    .and_then(|_| f.child_by_field_name("value"))
                    .and_then(|i| rust_ctor_type(i, src, self_class)),
                "scoped_identifier" => {
                    let name = text(f.child_by_field_name("name")?, src);
                    let ctor = name == "new" || name == "default" || name.starts_with("new_")
                        || name.starts_with("with_") || name.starts_with("from");
                    let path = f.child_by_field_name("path")?;
                    let ty = match path.kind() {
                        "identifier" if text(path, src) == "Self" => self_class,
                        "identifier" => Some(text(path, src)),
                        "scoped_identifier" => path.child_by_field_name("name").map(|n| text(n, src)),
                        _ => None,
                    };
                    ty.filter(|_| ctor)
                }
                _ => None,
            }
        }
        _ => None,
    }
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
                            // `self.foo()` in `impl T` -> T; `x.foo()` -> `ident:x` for the local
                            // binding post-pass (`let x = T::new()`, `x: &T`, ...)
                            let recv = f.child_by_field_name("value").and_then(|v| match v.kind() {
                                "self" => ctx.self_class.map(str::to_string),
                                "identifier" => Some(format!("ident:{}", text(v, src))),
                                _ => None,
                            });
                            push_call(out, text(field, src), "method", node, ctx.enclosing, recv);
                        }
                    }
                    // `T::y()` / `m::T::y()` name their type in the call itself — syntactically
                    // free (L1 rule), so T is the hint; `Self::y()` is the impl type. A module
                    // path (`fs::read`) hints a name that isn't one class symbol, which the
                    // resolver already distrusts -> same universal answer as no hint.
                    "scoped_identifier" => {
                        if let Some(name) = f.child_by_field_name("name") {
                            let recv = f.child_by_field_name("path").and_then(|p| match p.kind() {
                                "identifier" if text(p, src) == "Self" => ctx.self_class.map(str::to_string),
                                "identifier" => Some(text(p, src).to_string()),
                                "scoped_identifier" => p.child_by_field_name("name").map(|n| text(n, src).to_string()),
                                _ => None,
                            });
                            push_call(out, text(name, src), "method", node, ctx.enclosing, recv);
                        }
                    }
                    _ => {}
                }
            }
        }
        "let_declaration" | "parameter" => {
            if let Some(pat) = node.child_by_field_name("pattern").filter(|p| p.kind() == "identifier") {
                let ty = node.child_by_field_name("type").and_then(|t| rust_type_name(t, src)).or_else(|| {
                    node.child_by_field_name("value").and_then(|v| rust_ctor_type(v, src, ctx.self_class))
                });
                if let Some(t) = ty {
                    out.var_bindings.push((ctx.enclosing.to_string(), text(pat, src).to_string(), t.to_string()));
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
    walk_cpp(tree.root_node(), src.as_bytes(), &mut out, CppCtx { enclosing: "<module>", container: None, this_class: None });
    bind_local_receivers(&mut out);
    Ok(out)
}

#[derive(Clone, Copy)]
struct CppCtx<'a> {
    enclosing: &'a str,
    container: Option<&'a str>, // enclosing class/struct body
    /// what `this` names here: the class whose method (or field initializer) encloses this code.
    /// Syntactically free like Rust's `self`; JS `function` expressions rebind it (-> None).
    this_class: Option<&'a str>,
}

fn walk_cpp<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: CppCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "class_specifier" | "struct_specifier" => {
            // only a definition (name + body) is a def; bare `struct X` type references are not
            if let (Some(name), Some(_body)) = (node.child_by_field_name("name"), node.child_by_field_name("body")) {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, None));
                out.defs.last_mut().expect("just pushed").base_class = first_base_name(node, src);
                child_ctx = CppCtx { container: Some(nm), this_class: Some(nm), ..ctx };
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
                    child_ctx = CppCtx { enclosing: nm, container: None, this_class: parent };
                }
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, ctx.enclosing, None),
                    "field_expression" => {
                        if let Some(field) = f.child_by_field_name("field") {
                            let arg = f.child_by_field_name("argument");
                            let recv = this_hint(arg, ctx).or_else(|| ident_hint(arg, src));
                            push_call(out, text(field, src), "method", node, ctx.enclosing, recv);
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
        "declaration" | "parameter_declaration" => {
            let mut c = node.walk();
            let decls: Vec<Node> = node.children_by_field_name("declarator", &mut c).collect();
            let ty = node.child_by_field_name("type").filter(|t| t.kind() != "placeholder_type_specifier");
            push_cfam_bindings(out, ctx.enclosing, ty, &decls, src);
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
    walk_csharp(tree.root_node(), src.as_bytes(), &mut out, CppCtx { enclosing: "<module>", container: None, this_class: None });
    bind_local_receivers(&mut out);
    Ok(out)
}

fn walk_csharp<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: CppCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "class_declaration" | "interface_declaration" | "struct_declaration" | "record_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, leading_doc(node, src)));
                out.defs.last_mut().expect("just pushed").base_class = first_base_name(node, src);
                child_ctx = CppCtx { container: Some(nm), this_class: Some(nm), ..ctx };
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { enclosing: nm, container: None, this_class: ctx.container };
            }
        }
        "local_function_statement" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", None, node, src, None));
                child_ctx = CppCtx { enclosing: nm, container: None, ..ctx };
            }
        }
        "invocation_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, ctx.enclosing, None),
                    "member_access_expression" => {
                        if let Some(name) = f.child_by_field_name("name") {
                            let e = f.child_by_field_name("expression");
                            let recv = this_hint(e, ctx).or_else(|| ident_hint(e, src));
                            push_call(out, text(name, src), "method", node, ctx.enclosing, recv);
                        }
                    }
                    _ => {}
                }
            }
        }
        "local_declaration_statement" => {
            if let Some(vd) = find_child(node, "variable_declaration") {
                let mut c = vd.walk();
                let decls: Vec<Node> = vd.named_children(&mut c).filter(|k| k.kind() == "variable_declarator").collect();
                let ty = vd.child_by_field_name("type").filter(|t| t.kind() != "implicit_type");
                push_cfam_bindings(out, ctx.enclosing, ty, &decls, src);
            }
        }
        "parameter" => {
            if let Some(n) = node.child_by_field_name("name") {
                push_cfam_bindings(out, ctx.enclosing, node.child_by_field_name("type"), &[n], src);
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
    walk_java(tree.root_node(), src.as_bytes(), &mut out, CppCtx { enclosing: "<module>", container: None, this_class: None });
    bind_local_receivers(&mut out);
    Ok(out)
}

fn walk_java<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: CppCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, leading_doc(node, src)));
                out.defs.last_mut().expect("just pushed").base_class = first_base_name(node, src);
                child_ctx = CppCtx { container: Some(nm), this_class: Some(nm), ..ctx };
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { enclosing: nm, container: None, this_class: ctx.container };
            }
        }
        "method_invocation" => {
            if let Some(name) = node.child_by_field_name("name") {
                // `x.foo()` -> method; bare `foo()` -> func; `this.foo()` -> enclosing class hint
                let obj = node.child_by_field_name("object");
                let kind = if obj.is_some() { "method" } else { "func" };
                push_call(out, text(name, src), kind, node, ctx.enclosing, this_hint(obj, ctx).or_else(|| ident_hint(obj, src)));
            }
        }
        "local_variable_declaration" | "formal_parameter" => {
            let mut c = node.walk();
            let mut decls: Vec<Node> = node.children_by_field_name("declarator", &mut c).collect();
            if let Some(n) = node.child_by_field_name("name") {
                decls.push(n); // formal_parameter names the variable directly
            }
            push_cfam_bindings(out, ctx.enclosing, node.child_by_field_name("type"), &decls, src);
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
    walk_js(tree.root_node(), src.as_bytes(), &mut out, CppCtx { enclosing: "<module>", container: None, this_class: None });
    // a `global:X` hint only stands if this file never declares its own `X` (then it's that binding);
    // an `ident:X` receiver becomes `jsmod:<spec>` when X is an `import * as X from spec` binding
    // the file doesn't also declare some other way, else no hint
    let declared = js_declared_names(tree.root_node(), src.as_bytes(), true);
    let declared_locally = js_declared_names(tree.root_node(), src.as_bytes(), false);
    let namespaces = js_namespace_imports(tree.root_node(), src.as_bytes());
    for c in &mut out.calls {
        let Some(r) = c.receiver_class.as_deref() else { continue };
        if let Some(g) = r.strip_prefix("global:") {
            if declared.contains(g) {
                c.receiver_class = None;
            }
        } else if let Some(x) = r.strip_prefix("ident:") {
            // a namespace import wins; otherwise leave `ident:` for the local-binding pass
            if let Some(spec) = namespaces.get(x).filter(|_| !declared_locally.contains(x)) {
                c.receiver_class = Some(format!("jsmod:{spec}"));
            }
        }
    }
    // TS type-checks every reassignment against the declared / inferred type, so `x = null` or
    // `x = factory()` can't change it: only a reassignment to a different `new T()` still counts
    // (conservative about runtime dispatch). Plain JS has no static type: every reassignment counts.
    let typescript = what != "javascript";
    out.var_bindings.retain_mut(|(_, _, t)| match t.strip_prefix('=') {
        Some("?") if typescript => false,
        Some("!") => {
            *t = "?".to_string(); // destructuring write: void in TS too
            true
        }
        Some(rest) => {
            *t = rest.to_string();
            true
        }
        None => true,
    });
    bind_local_receivers(&mut out);
    Ok(out)
}

/// A TS type annotation / `new` target as a plain class name: `T`, `ns.T`. Generics, unions,
/// arrays, primitives -> None.
fn js_type_name<'a>(t: Node, src: &'a [u8]) -> Option<&'a str> {
    match t.kind() {
        "type_annotation" => t.named_child(0).and_then(|i| js_type_name(i, src)),
        "type_identifier" | "identifier" => Some(text(t, src)),
        "nested_type_identifier" => t.child_by_field_name("name").map(|n| text(n, src)),
        "member_expression" => t.child_by_field_name("property").map(|n| text(n, src)),
        _ => None,
    }
}

/// Built-in global objects of the JS runtimes (browser + Node): `Promise.all()`, `Math.max()`,
/// `console.log()` can only be repo code if the file shadows the name, which `js_declared_names`
/// rules out.
const JS_GLOBALS: &[&str] = &[
    "Promise", "Math", "JSON", "Object", "Array", "console", "Number", "String", "Boolean", "Date", "Reflect",
    "Symbol", "Intl", "BigInt", "Atomics", "Error", "RegExp", "Map", "Set", "WeakMap", "WeakSet", "Proxy",
    "globalThis", "window", "document", "navigator", "location", "history", "localStorage", "sessionStorage",
    "process", "Buffer", "crypto", "performance", "URL",
];

/// `import * as ns from "spec"` bindings: ns -> spec.
fn js_namespace_imports(root: Node, src: &[u8]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "import_statement" {
            if let (Some(source), Some(ns)) = (n.child_by_field_name("source"), find_descendant(n, "namespace_import")) {
                if let Some(id) = find_child(ns, "identifier") {
                    let spec = text(source, src).trim_matches(|c| c == '"' || c == '\'' || c == '`');
                    map.insert(text(id, src).to_string(), spec.to_string());
                }
            }
            continue;
        }
        let mut c = n.walk();
        stack.extend(n.named_children(&mut c));
    }
    map
}

fn find_descendant<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        if x.kind() == kind {
            return Some(x);
        }
        let mut c = x.walk();
        stack.extend(x.named_children(&mut c));
    }
    None
}

/// Every name this file declares anywhere (variables incl. destructuring, parameters, catch params,
/// function/class/enum names, and imports when `with_imports`) — scope-blind on purpose: any
/// declaration of `Math` anywhere in the file drops every `global:Math` hint in it (conservative).
fn js_declared_names(root: Node, src: &[u8], with_imports: bool) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let decl = match n.kind() {
            "variable_declarator" => n.child_by_field_name("name"),
            "formal_parameters" => Some(n),
            "import_clause" if with_imports => Some(n),
            "catch_clause" => n.child_by_field_name("parameter"),
            "function_declaration" | "generator_function_declaration" | "class_declaration"
            | "abstract_class_declaration" | "enum_declaration" => n.child_by_field_name("name"),
            _ => None,
        };
        if let Some(d) = decl {
            let mut inner = vec![d];
            while let Some(x) = inner.pop() {
                if matches!(x.kind(), "identifier" | "type_identifier" | "shorthand_property_identifier_pattern") {
                    names.insert(text(x, src).to_string());
                }
                let mut c = x.walk();
                inner.extend(x.named_children(&mut c));
            }
        }
        let mut c = n.walk();
        stack.extend(n.named_children(&mut c));
    }
    names
}

fn walk_js<'a>(node: Node, src: &'a [u8], out: &mut ParsedFile, ctx: CppCtx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", None, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { enclosing: nm, container: None, this_class: None };
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "class", ctx.container, node, src, leading_doc(node, src)));
                out.defs.last_mut().expect("just pushed").base_class = first_base_name(node, src);
                child_ctx = CppCtx { container: Some(nm), this_class: Some(nm), ..ctx };
            }
        }
        "method_definition" => {
            if let Some(name) = node.child_by_field_name("name").filter(|n| n.kind() == "property_identifier") {
                let nm = text(name, src);
                out.defs.push(mk_def(nm, "function", ctx.container, node, src, leading_doc(node, src)));
                child_ctx = CppCtx { enclosing: nm, container: None, this_class: ctx.container };
            }
        }
        // `x = ...` later in the function: dispatch is on the runtime object, so a reassignment is a
        // binding too — `new T()` binds T, anything else "?" (disagrees with every type -> no hint).
        // Marked "=" so parse_js_family can drop the "?" ones for TS (see there).
        "assignment_expression" => match node.child_by_field_name("left") {
            Some(l) if l.kind() == "identifier" => {
                let ty = node
                    .child_by_field_name("right")
                    .filter(|r| r.kind() == "new_expression")
                    .and_then(|r| r.child_by_field_name("constructor"))
                    .and_then(|c| js_type_name(c, src))
                    .unwrap_or("?");
                out.var_bindings.push((ctx.enclosing.to_string(), text(l, src).to_string(), format!("={ty}")));
            }
            // `({ x } = ..)` / `[x] = ..`: every name it writes voids its hint, JS and TS alike
            // ("=!"; destructuring reassignment of a typed local is rare, so strict is cheap)
            Some(l) if matches!(l.kind(), "object_pattern" | "array_pattern") => {
                let mut stack = vec![l];
                while let Some(x) = stack.pop() {
                    if matches!(x.kind(), "identifier" | "shorthand_property_identifier_pattern") {
                        out.var_bindings.push((ctx.enclosing.to_string(), text(x, src).to_string(), "=!".into()));
                    }
                    let mut c = x.walk();
                    stack.extend(x.named_children(&mut c));
                }
            }
            _ => {}
        },
        // `(x: T)` params (TS)
        "required_parameter" | "optional_parameter" => {
            if let (Some(p), Some(t)) = (
                node.child_by_field_name("pattern").filter(|p| p.kind() == "identifier"),
                node.child_by_field_name("type").and_then(|t| js_type_name(t, src)),
            ) {
                out.var_bindings.push((ctx.enclosing.to_string(), text(p, src).to_string(), t.to_string()));
            }
        }
        // `const x = () => ..` / `const x = function ..` — cheap and very common (spec L1.3)
        "variable_declarator" => {
            // local binding for receiver narrowing: `const x: T = ..` or `const x = new T(..)`
            if let Some(name) = node.child_by_field_name("name").filter(|n| n.kind() == "identifier") {
                let ty = node.child_by_field_name("type").and_then(|t| js_type_name(t, src)).or_else(|| {
                    node.child_by_field_name("value")
                        .filter(|v| v.kind() == "new_expression")
                        .and_then(|v| v.child_by_field_name("constructor"))
                        .and_then(|c| js_type_name(c, src))
                });
                if let Some(t) = ty {
                    out.var_bindings.push((ctx.enclosing.to_string(), text(name, src).to_string(), t.to_string()));
                }
            }
            if let (Some(name), Some(value)) = (node.child_by_field_name("name"), node.child_by_field_name("value")) {
                if name.kind() == "identifier"
                    && matches!(value.kind(), "arrow_function" | "function_expression" | "function")
                {
                    let nm = text(name, src);
                    out.defs.push(mk_def(nm, "function", None, node, src, None));
                    child_ctx = CppCtx { enclosing: nm, container: None, ..ctx };
                }
            }
        }
        // an anonymous `function () {}` / generator gets its own `this`
        "function_expression" | "function" | "generator_function" => {
            child_ctx = CppCtx { this_class: None, ..ctx };
        }
        // an object literal's methods belong to (and `this` is) the object, not an enclosing class
        // — e.g. `class F { handlers = { h() { this.x() } } }`
        "object" => {
            child_ctx = CppCtx { container: None, this_class: None, ..ctx };
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => push_call(out, text(f, src), "func", node, ctx.enclosing, None),
                    "member_expression" => {
                        if let Some(prop) = f.child_by_field_name("property").filter(|p| p.kind() == "property_identifier") {
                            let obj = f.child_by_field_name("object");
                            // `this` -> class; a global object -> `global:`; any other plain identifier ->
                            // `ident:` for the namespace-import post-pass (never reaches the store)
                            let recv = this_hint(obj, ctx).or_else(|| {
                                obj.filter(|o| o.kind() == "identifier").map(|o| text(o, src)).map(|t| {
                                    if JS_GLOBALS.contains(&t) { format!("global:{t}") } else { format!("ident:{t}") }
                                })
                            });
                            push_call(out, text(prop, src), "method", node, ctx.enclosing, recv);
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
