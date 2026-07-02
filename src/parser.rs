//! S1.1/S1.3/S2/L1.1 — parse one source file into syntactic facts, plus the language registry.
//!
//! Extracts: definitions (fn/class, with parent_class for methods), call-sites (with enclosing
//! function, func/method kind, and a deterministic receiver-class hint), imports and import aliases.
//! The receiver-class hint powers the S2 Python resolver: `self.foo()` -> enclosing class;
//! `x = ClassName(); x.foo()` or `ClassName().foo()` -> ClassName (validated against the symbol
//! table at resolution time). Manual node-walk over the stable Node API.
//!
//! L1.1: `LANGS` maps file extensions -> (language name, walk fn). Python's walk lives here
//! (it alone feeds the S2 exact resolver); the 8 universal-tier walks live in `crate::langs`.

use serde::Serialize;
use std::path::Path;
use tree_sitter::{Node, Parser};

/// L1.1 — one registered language: its `lang` column value, the extensions it claims, and the walk
/// producing the shared `ParsedFile` shape. The tree-sitter `Language` is owned by the walk fn.
#[derive(Debug)]
pub struct LangSpec {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub parse: fn(&str) -> anyhow::Result<ParsedFile>,
}

/// The registry. `.ts` and `.tsx` are separate grammars but ONE language ("typescript") — a `.ts`
/// caller may resolve into a `.tsx` def and vice versa. JS and TS stay separate languages.
pub static LANGS: &[LangSpec] = &[
    LangSpec { name: "python", extensions: &["py"], parse: parse_python },
    LangSpec { name: "rust", extensions: &["rs"], parse: crate::langs::parse_rust },
    LangSpec { name: "c", extensions: &["c", "h"], parse: crate::langs::parse_c },
    LangSpec { name: "cpp", extensions: &["cpp", "cc", "hpp", "hh"], parse: crate::langs::parse_cpp },
    LangSpec { name: "csharp", extensions: &["cs"], parse: crate::langs::parse_csharp },
    LangSpec { name: "java", extensions: &["java"], parse: crate::langs::parse_java },
    LangSpec { name: "javascript", extensions: &["js", "jsx", "mjs", "cjs"], parse: crate::langs::parse_javascript },
    LangSpec { name: "typescript", extensions: &["ts"], parse: crate::langs::parse_typescript },
    LangSpec { name: "typescript", extensions: &["tsx"], parse: crate::langs::parse_tsx },
    LangSpec { name: "go", extensions: &["go"], parse: crate::langs::parse_go },
];

pub fn lang_for_path(path: &Path) -> Option<&'static LangSpec> {
    let ext = path.extension()?.to_str()?;
    LANGS.iter().find(|l| l.extensions.contains(&ext))
}

pub fn is_registered_ext(ext: &str) -> bool {
    LANGS.iter().any(|l| l.extensions.contains(&ext))
}

/// ".py .rs .c ..." — for the `maple parse` unsupported-language error.
pub fn supported_extensions_list() -> String {
    LANGS.iter().flat_map(|l| l.extensions).map(|e| format!(".{e}")).collect::<Vec<_>>().join(" ")
}

#[derive(Serialize, Debug)]
pub struct Definition {
    pub name: String,
    pub kind: String, // "function" | "class"
    pub parent_class: Option<String>, // Some(C) iff this def's immediate parent block is class C's body
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    /// T1: simplify(return annotation) for a function def; None for classes or unannotated/complex returns.
    pub ret_class: Option<String>,
    /// T4: the single plain-identifier superclass, iff the superclass list is exactly one identifier
    /// (qualified names, generics, multiple bases, metaclass kwargs -> None). Classes only.
    pub base_class: Option<String>,
    /// T10.2 (D4): first statement = a bare string literal -> its first line, ≤120 chars. None if
    /// the def has no leading string statement (never guess; empty first line -> None too).
    pub docstring: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct CallSite {
    pub name: String,
    pub kind: String,      // "func" (foo()) | "method" (x.foo())
    pub line: usize,
    pub enclosing: String, // nearest enclosing function name, or "<module>"
    /// Deterministic hint: "believed to be an instance of this class" (self -> enclosing method's
    /// class; var/ctor -> ClassName). Validated (must uniquely name a class) at resolution time.
    pub receiver_class: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct Import {
    pub raw: String,
    pub line: usize,
}

#[derive(Serialize, Debug)]
pub struct Alias {
    pub local: String,
    pub source: String,
}

/// T3: a structured `from source_module import local` name (post-`as`, pre-alias-expansion source).
#[derive(Serialize, Debug)]
pub struct ImportName {
    pub local: String,
    pub source_module: String,
}

#[derive(Serialize, Debug, Default)]
pub struct ParsedFile {
    pub defs: Vec<Definition>,
    pub calls: Vec<CallSite>,
    pub imports: Vec<Import>,
    pub aliases: Vec<Alias>,
    pub import_names: Vec<ImportName>,
}

/// walk context: enclosing fn (call attribution), method_class (set only for the immediate class
/// body — a def nested in a method is NOT a method), self_class (propagates through nested defs,
/// since `self` stays in scope via closure).
#[derive(Clone, Copy)]
struct Ctx<'a> {
    enclosing_fn: &'a str,
    method_class: Option<&'a str>,
    self_class: Option<&'a str>,
}

/// raw var binding `x = ClassName(...)` inside a function body (also carries T1 param/return
/// annotation bindings — same shape, same narrowing post-pass).
struct Binding {
    fn_name: String,
    var: String,
    class_name: String,
}

/// T2: `self.attr = ClassName(...)` inside class C's methods -> class-scoped attribute binding.
struct AttrBinding {
    class_name: String,
    attr: String,
    target: String,
}

/// F8 — `walk`'s mutable accumulators, bundled into one struct so the function stays under
/// clippy's too_many_arguments threshold. Each field is exactly the accumulator it replaces.
struct WalkState<'o> {
    out: &'o mut ParsedFile,
    bindings: &'o mut Vec<Binding>,
    var_receivers: &'o mut Vec<(usize, String)>,
    attr_bindings: &'o mut Vec<AttrBinding>,
    attr_receivers: &'o mut Vec<(usize, String, String)>,
}

pub fn parse_python(source: &str) -> anyhow::Result<ParsedFile> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| anyhow::anyhow!("load python grammar: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter returned no tree"))?;
    let mut out = ParsedFile::default();
    let mut bindings: Vec<Binding> = Vec::new();
    // raw receiver var names for post-binding: (call index in out.calls, var name)
    let mut var_receivers: Vec<(usize, String)> = Vec::new();
    let mut attr_bindings: Vec<AttrBinding> = Vec::new();
    // (call index, enclosing self_class, attr name) for `self.attr.foo()` sites
    let mut attr_receivers: Vec<(usize, String, String)> = Vec::new();
    let mut state = WalkState {
        out: &mut out,
        bindings: &mut bindings,
        var_receivers: &mut var_receivers,
        attr_bindings: &mut attr_bindings,
        attr_receivers: &mut attr_receivers,
    };
    walk(
        tree.root_node(),
        source.as_bytes(),
        &mut state,
        Ctx { enclosing_fn: "<module>", method_class: None, self_class: None },
    );

    // T1.2: a ctor-style binding `(fn, x, Name)` where `Name` is a same-file function def with
    // ret_class=Some(R) narrows to `(fn, x, R)` — deterministic, same-file only.
    for b in bindings.iter_mut() {
        if let Some(r) = out
            .defs
            .iter()
            .find(|d| d.kind == "function" && d.name == b.class_name && d.ret_class.is_some())
        {
            b.class_name = r.ret_class.clone().unwrap();
        }
    }

    // post-pass: `x.foo()` where x was bound to exactly ONE distinct class name in the same fn
    // (ctor assignment, a type-annotated parameter, or a same-file return type — same binding pool)
    for (idx, var) in var_receivers {
        let fn_name = out.calls[idx].enclosing.clone();
        let mut classes: Vec<&str> = bindings
            .iter()
            .filter(|b| b.fn_name == fn_name && b.var == var)
            .map(|b| b.class_name.as_str())
            .collect();
        classes.sort();
        classes.dedup();
        if classes.len() == 1 {
            out.calls[idx].receiver_class = Some(classes[0].to_string());
        } // >1 distinct bindings or none -> no hint (never guess)
    }

    // T2 post-pass: `self.attr.foo()` where `attr` was bound to exactly ONE distinct class name
    // across all of the enclosing class's methods.
    for (idx, class_name, attr) in attr_receivers {
        let mut classes: Vec<&str> = attr_bindings
            .iter()
            .filter(|b| b.class_name == class_name && b.attr == attr)
            .map(|b| b.target.as_str())
            .collect();
        classes.sort();
        classes.dedup();
        if classes.len() == 1 {
            out.calls[idx].receiver_class = Some(classes[0].to_string());
        }
    }
    Ok(out)
}

pub(crate) fn text<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

pub(crate) fn first_line(n: Node, src: &[u8]) -> String {
    text(n, src).lines().next().unwrap_or("").trim_end().to_string()
}

/// T10.2 (D4): `body`'s first statement, if it's a bare string-literal expression statement, is the
/// docstring — first line only, ≤120 chars (a preview, not the whole thing). Anything else (no
/// leading string, f-string, empty first line) -> None; never guess.
fn extract_docstring(body: Node, src: &[u8]) -> Option<String> {
    let mut cursor = body.walk();
    let first = body.named_children(&mut cursor).next()?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let s = first.named_child(0)?;
    if s.kind() != "string" {
        return None;
    }
    let mut sc = s.walk();
    let content = s.named_children(&mut sc).find(|c| c.kind() == "string_content")?;
    let line = text(content, src).lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    Some(if line.chars().count() > 120 { line.chars().take(120).collect() } else { line.to_string() })
}

/// if `n` is a call whose function is a bare identifier, return that identifier (ctor pattern)
fn ctor_name<'a>(n: Node, src: &'a [u8]) -> Option<&'a str> {
    if n.kind() != "call" {
        return None;
    }
    let f = n.child_by_field_name("function")?;
    (f.kind() == "identifier").then(|| text(f, src))
}

fn walk<'a>(node: Node, src: &'a [u8], state: &mut WalkState, ctx: Ctx<'a>) {
    let mut child_ctx = ctx;
    match node.kind() {
        "function_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                let ret_class =
                    node.child_by_field_name("return_type").and_then(|t| simplify_type(t, src));
                let docstring = node.child_by_field_name("body").and_then(|b| extract_docstring(b, src));
                state.out.defs.push(Definition {
                    name: nm.to_string(),
                    kind: "function".into(),
                    parent_class: ctx.method_class.map(|c| c.to_string()),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    signature: first_line(node, src),
                    ret_class,
                    base_class: None,
                    docstring,
                });
                child_ctx = Ctx {
                    enclosing_fn: nm,
                    method_class: None, // defs nested inside this fn are not methods
                    // entering a method binds self to its class; nested defs keep it (closure)
                    self_class: ctx.method_class.or(ctx.self_class),
                };
            }
        }
        "class_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                let nm = text(name, src);
                // T4: base_class only when the superclass list is EXACTLY one plain identifier
                // (multiple bases, qualified names, generics, metaclass kwargs -> None)
                let base_class = node.child_by_field_name("superclasses").and_then(|sc| {
                    let mut cur = sc.walk();
                    let children: Vec<Node> = sc.named_children(&mut cur).collect();
                    (children.len() == 1 && children[0].kind() == "identifier")
                        .then(|| text(children[0], src).to_string())
                });
                let docstring = node.child_by_field_name("body").and_then(|b| extract_docstring(b, src));
                state.out.defs.push(Definition {
                    name: nm.to_string(),
                    kind: "class".into(),
                    parent_class: ctx.method_class.map(|c| c.to_string()),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    signature: first_line(node, src),
                    ret_class: None,
                    base_class,
                    docstring,
                });
                child_ctx = Ctx { method_class: Some(nm), ..ctx };
            }
        }
        "typed_parameter" => {
            // `x: T` in a function's own parameter list -> binding (fn, x, simplify(T)) (T1)
            if let (Some(name_node), Some(type_node)) =
                (node.named_child(0), node.child_by_field_name("type"))
            {
                if name_node.kind() == "identifier" {
                    if let Some(cls) = simplify_type(type_node, src) {
                        state.bindings.push(Binding {
                            fn_name: ctx.enclosing_fn.to_string(),
                            var: text(name_node, src).to_string(),
                            class_name: cls,
                        });
                    }
                }
            }
        }
        "typed_default_parameter" => {
            // `x: T = default` -> same binding as typed_parameter (T1)
            if let (Some(name_node), Some(type_node)) =
                (node.child_by_field_name("name"), node.child_by_field_name("type"))
            {
                if let Some(cls) = simplify_type(type_node, src) {
                    state.bindings.push(Binding {
                        fn_name: ctx.enclosing_fn.to_string(),
                        var: text(name_node, src).to_string(),
                        class_name: cls,
                    });
                }
            }
        }
        "assignment" => {
            if let (Some(left), Some(right)) =
                (node.child_by_field_name("left"), node.child_by_field_name("right"))
            {
                if left.kind() == "identifier" {
                    // x = ClassName(...)  ->  binding hint for `x.method()` later in the same fn
                    if let Some(cls) = ctor_name(right, src) {
                        state.bindings.push(Binding {
                            fn_name: ctx.enclosing_fn.to_string(),
                            var: text(left, src).to_string(),
                            class_name: cls.to_string(),
                        });
                    }
                } else if left.kind() == "attribute" {
                    // self.attr = ClassName(...) -> class-scoped binding (T2)
                    if let (Some(obj), Some(attr)) =
                        (left.child_by_field_name("object"), left.child_by_field_name("attribute"))
                    {
                        if obj.kind() == "identifier" && text(obj, src) == "self" {
                            if let Some(class_name) = ctx.self_class {
                                if let Some(cls) = ctor_name(right, src) {
                                    state.attr_bindings.push(AttrBinding {
                                        class_name: class_name.to_string(),
                                        attr: text(attr, src).to_string(),
                                        target: cls.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        "call" => {
            if let Some(func) = node.child_by_field_name("function") {
                match func.kind() {
                    "identifier" => state.out.calls.push(CallSite {
                        name: text(func, src).to_string(),
                        kind: "func".into(),
                        line: node.start_position().row + 1,
                        enclosing: ctx.enclosing_fn.to_string(),
                        receiver_class: None,
                    }),
                    "attribute" => {
                        let name = func
                            .child_by_field_name("attribute")
                            .map(|a| text(a, src).to_string())
                            .unwrap_or_default();
                        if !name.is_empty() {
                            let obj = func.child_by_field_name("object");
                            let mut receiver_class: Option<String> = None;
                            let mut var: Option<String> = None;
                            let mut attr_recv: Option<(String, String)> = None;
                            if let Some(o) = obj {
                                match o.kind() {
                                    "identifier" => {
                                        let t = text(o, src);
                                        if t == "self" {
                                            receiver_class = ctx.self_class.map(|c| c.to_string());
                                        } else {
                                            var = Some(t.to_string());
                                        }
                                    }
                                    "call" => {
                                        receiver_class = ctor_name(o, src).map(|c| c.to_string());
                                    }
                                    "attribute" => {
                                        // self.attr.method() -> deferred class-scoped lookup (T2)
                                        if let (Some(oo), Some(oa)) = (
                                            o.child_by_field_name("object"),
                                            o.child_by_field_name("attribute"),
                                        ) {
                                            if oo.kind() == "identifier" && text(oo, src) == "self" {
                                                if let Some(cn) = ctx.self_class {
                                                    attr_recv = Some((
                                                        cn.to_string(),
                                                        text(oa, src).to_string(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            state.out.calls.push(CallSite {
                                name,
                                kind: "method".into(),
                                line: node.start_position().row + 1,
                                enclosing: ctx.enclosing_fn.to_string(),
                                receiver_class,
                            });
                            if let Some(v) = var {
                                state.var_receivers.push((state.out.calls.len() - 1, v));
                            }
                            if let Some((cn, at)) = attr_recv {
                                state.attr_receivers.push((state.out.calls.len() - 1, cn, at));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        "aliased_import" => {
            if let (Some(name), Some(alias)) =
                (node.child_by_field_name("name"), node.child_by_field_name("alias"))
            {
                let source = text(name, src).rsplit('.').next().unwrap_or("").to_string();
                state.out.aliases.push(Alias { local: text(alias, src).to_string(), source });
            }
        }
        "import_statement" => {
            state.out.imports.push(Import { raw: first_line(node, src), line: node.start_position().row + 1 });
            // module imports (`import a.b`) are out of scope for T3 (module-attribute calls are
            // "method" kind) -> no import_names emitted here.
        }
        "import_from_statement" => {
            state.out.imports.push(Import { raw: first_line(node, src), line: node.start_position().row + 1 });
            let mut wc = node.walk();
            let is_wildcard = node.named_children(&mut wc).any(|c| c.kind() == "wildcard_import");
            if !is_wildcard {
                if let Some(module_node) = node.child_by_field_name("module_name") {
                    // relative imports (`from . import x`) have no dotted module name -> skip
                    if module_node.kind() == "dotted_name" {
                        let module = text(module_node, src).to_string();
                        let mut nc = node.walk();
                        for name_node in node.children_by_field_name("name", &mut nc) {
                            match name_node.kind() {
                                "dotted_name" => state.out.import_names.push(ImportName {
                                    local: text(name_node, src).to_string(),
                                    source_module: module.clone(),
                                }),
                                "aliased_import" => {
                                    if let Some(alias) = name_node.child_by_field_name("alias") {
                                        state.out.import_names.push(ImportName {
                                            local: text(alias, src).to_string(),
                                            source_module: module.clone(),
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, src, state, child_ctx);
    }
}

/// T1: simplify a `type` field node (tree-sitter-python's `type` supertype wraps the concrete
/// node) into a plain class name, per the deterministic narrowing rules — anything else (generics
/// other than `Optional[X]`, multi-real-type unions, qualified/member types, empty strings) -> None.
fn simplify_type(type_node: Node, src: &[u8]) -> Option<String> {
    let inner = type_node.named_child(0)?;
    match inner.kind() {
        "identifier" => Some(text(inner, src).to_string()),
        "string" => {
            let mut cursor = inner.walk();
            let content = inner.named_children(&mut cursor).find(|c| c.kind() == "string_content")?;
            let s = text(content, src);
            is_plain_identifier(s).then(|| s.to_string())
        }
        "generic_type" => {
            let base = inner.named_child(0)?;
            if base.kind() != "identifier" || text(base, src) != "Optional" {
                return None; // list[X], Dict[X,Y], etc. -> no binding
            }
            let type_param = inner.named_child(1)?;
            if type_param.kind() != "type_parameter" || type_param.named_child_count() != 1 {
                return None;
            }
            let arg = type_param.named_child(0)?; // a `type` wrapper
            let arg_inner = arg.named_child(0)?;
            (arg_inner.kind() == "identifier").then(|| text(arg_inner, src).to_string())
        }
        "binary_operator" => {
            // only `X | None` / `None | X` — 2+ real-type unions or non-`|` ops -> None
            let op = inner.child_by_field_name("operator")?;
            if op.kind() != "|" {
                return None;
            }
            let left = inner.child_by_field_name("left")?;
            let right = inner.child_by_field_name("right")?;
            match (left.kind(), right.kind()) {
                ("identifier", "none") => Some(text(left, src).to_string()),
                ("none", "identifier") => Some(text(right, src).to_string()),
                _ => None,
            }
        }
        _ => None, // attribute/member_type (a.B), subscript (typing.Optional[X]), etc. -> no binding
    }
}

fn is_plain_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_defs_calls_enclosing_aliases() {
        let src = "\
import os
from a.b import bar as baz

def helper(x):
    return x + 1

class Thing:
    def method(self):
        return self.helper(baz())

def target(y):
    return helper(y)
";
        let p = parse_python(src).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "target" && d.kind == "function" && d.parent_class.is_none()));
        assert!(p.defs.iter().any(|d| d.name == "Thing" && d.kind == "class"));
        // method def carries its class
        assert!(p.defs.iter().any(|d| d.name == "method" && d.parent_class.as_deref() == Some("Thing")));
        assert!(p.calls.iter().any(|c| c.name == "helper" && c.kind == "func" && c.enclosing == "target"));
        assert!(p.calls.iter().any(|c| c.name == "baz" && c.enclosing == "method"));
        // self.helper(...) -> method kind with receiver_class Thing (S2 hint)
        assert!(p
            .calls
            .iter()
            .any(|c| c.name == "helper" && c.kind == "method" && c.receiver_class.as_deref() == Some("Thing")));
        assert!(p.aliases.iter().any(|a| a.local == "baz" && a.source == "bar"));
    }

    #[test]
    fn receiver_class_hints() {
        let src = "\
class A:
    def foo(self):
        return 1

class B:
    def foo(self):
        return self.foo()

def use():
    a = A()
    a.foo()
    A().foo()
    b = unknown()
    b.foo()

def rebound():
    x = A()
    x = B()
    x.foo()

def nested_self():
    pass

class C:
    def outer(self):
        def inner():
            return self.foo()
        return inner
";
        let p = parse_python(src).unwrap();
        let find = |encl: &str, line_hint: Option<usize>| {
            p.calls
                .iter()
                .filter(|c| c.name == "foo" && c.enclosing == encl)
                .filter(|c| line_hint.is_none_or(|l| c.line == l))
                .collect::<Vec<_>>()
        };
        // self.foo() in B.foo -> receiver_class B
        assert!(find("foo", None).iter().any(|c| c.receiver_class.as_deref() == Some("B")));
        // a = A(); a.foo() -> A ; A().foo() -> A ; b = unknown() -> hint recorded, validated at
        // resolution time (store checks the hint names exactly one class symbol)
        let in_use = find("use", None);
        assert_eq!(in_use.iter().filter(|c| c.receiver_class.as_deref() == Some("A")).count(), 2);
        assert!(in_use.iter().any(|c| c.receiver_class.as_deref() == Some("unknown")));
        // rebound to two different classes -> no hint (never guess)
        assert!(find("rebound", None).iter().all(|c| c.receiver_class.is_none()));
        // self inside nested fn still binds to C (closure)
        assert!(find("inner", None).iter().any(|c| c.receiver_class.as_deref() == Some("C")));
    }

    /// T1 — param type-annotation bindings: plain, Optional[X], X|None, None|X, forward-ref string
    /// all narrow; generics and unannotated params do not (never guess).
    #[test]
    fn t1_param_annotation_bindings() {
        let src = "\
class A:
    def foo(self):
        return 1

class B:
    def foo(self):
        return 2

def use_plain(a: A):
    a.foo()

def use_optional(a: Optional[A]):
    a.foo()

def use_union(a: A | None):
    a.foo()

def use_union_rev(a: None | A):
    a.foo()

def use_forward(a: 'A'):
    a.foo()

def use_list(a: list[A]):
    a.foo()

def use_bare(a):
    a.foo()
";
        let p = parse_python(src).unwrap();
        let hint = |encl: &str| {
            p.calls.iter().find(|c| c.name == "foo" && c.enclosing == encl).unwrap().receiver_class.clone()
        };
        assert_eq!(hint("use_plain"), Some("A".to_string()));
        assert_eq!(hint("use_optional"), Some("A".to_string()));
        assert_eq!(hint("use_union"), Some("A".to_string()));
        assert_eq!(hint("use_union_rev"), Some("A".to_string()));
        assert_eq!(hint("use_forward"), Some("A".to_string()));
        assert_eq!(hint("use_list"), None, "generic list[X] -> no binding");
        assert_eq!(hint("use_bare"), None, "unannotated param -> no binding, stays ambiguous downstream");
    }

    /// T1 — same-file return-type binding: `def make() -> A` narrows `x = make(); x.foo()`.
    #[test]
    fn t1_return_annotation_same_file() {
        let src = "\
class A:
    def foo(self):
        return 1

def make() -> A:
    return A()

def use():
    x = make()
    x.foo()
";
        let p = parse_python(src).unwrap();
        assert_eq!(
            p.defs.iter().find(|d| d.name == "make").unwrap().ret_class.as_deref(),
            Some("A")
        );
        let call = p.calls.iter().find(|c| c.name == "foo" && c.enclosing == "use").unwrap();
        assert_eq!(call.receiver_class.as_deref(), Some("A"));
    }

    /// T2 — self.attr = ClassName() bindings narrow self.attr.method(); rebinding to 2+ distinct
    /// classes across methods keeps it unresolved-hint (never guess).
    #[test]
    fn t2_self_attr_bindings() {
        let src = "\
class A:
    def foo(self):
        return 1

class B:
    def foo(self):
        return 2

class C:
    def __init__(self):
        self.p = A()

    def m(self):
        self.p.foo()

class D:
    def one(self):
        self.q = A()

    def two(self):
        self.q = B()

    def use(self):
        self.q.foo()
";
        let p = parse_python(src).unwrap();
        let hint = |encl: &str| {
            p.calls.iter().find(|c| c.name == "foo" && c.enclosing == encl).unwrap().receiver_class.clone()
        };
        assert_eq!(hint("m"), Some("A".to_string()));
        assert_eq!(hint("use"), None, "attr rebound to A and B across methods -> no hint");
    }

    /// T3 — structured import names feed the store's import-aware `func` resolution; module
    /// imports (`import a.b`) and wildcard imports are out of scope and emit no ImportName.
    #[test]
    fn t3_import_names() {
        let src = "\
from a.b import c
from a.b import c as d
from x import *
import a.b
from a.b import e, f as g
";
        let p = parse_python(src).unwrap();
        assert!(p.import_names.iter().any(|n| n.local == "c" && n.source_module == "a.b"));
        assert!(p.import_names.iter().any(|n| n.local == "d" && n.source_module == "a.b"));
        assert!(p.import_names.iter().any(|n| n.local == "e" && n.source_module == "a.b"));
        assert!(p.import_names.iter().any(|n| n.local == "g" && n.source_module == "a.b"));
        assert_eq!(p.import_names.len(), 4, "wildcard and plain module imports emit no ImportName");
    }

    /// T10.2 — docstring: first statement = bare string literal -> first line, ≤120 chars; anything
    /// else (no leading string, blank first line) -> None (never guess). Functions and classes both.
    #[test]
    fn t10_docstring_extraction() {
        let src = "\
def documented():
    \"\"\"First line.

    More stuff that should be dropped.
    \"\"\"
    return 1

def undocumented():
    return 1

class Widget:
    \"\"\"A widget.\"\"\"
    def method(self):
        return 1
";
        let p = parse_python(src).unwrap();
        let doc = |n: &str| p.defs.iter().find(|d| d.name == n).unwrap().docstring.clone();
        assert_eq!(doc("documented"), Some("First line.".to_string()));
        assert_eq!(doc("undocumented"), None);
        assert_eq!(doc("Widget"), Some("A widget.".to_string()));
        assert_eq!(doc("method"), None);

        let long_src = format!("def long_doc():\n    \"\"\"{}\n    more\n    \"\"\"\n    return 1\n", "x".repeat(200));
        let p2 = parse_python(&long_src).unwrap();
        let long = p2.defs.iter().find(|d| d.name == "long_doc").unwrap().docstring.clone().unwrap();
        assert_eq!(long.chars().count(), 120, "truncated to <=120 chars");
    }

    /// T4 — base_class set only for exactly-one-plain-identifier superclass lists.
    #[test]
    fn t4_base_class() {
        let src = "\
class Base:
    pass

class C(Base):
    pass

class D(pkg.Base):
    pass

class E(Base, Other):
    pass

class F(Base, metaclass=M):
    pass

class G:
    pass
";
        let p = parse_python(src).unwrap();
        let base_of = |n: &str| p.defs.iter().find(|d| d.name == n).unwrap().base_class.clone();
        assert_eq!(base_of("C"), Some("Base".to_string()));
        assert_eq!(base_of("D"), None, "qualified name -> no binding");
        assert_eq!(base_of("E"), None, "multiple bases -> no binding");
        assert_eq!(base_of("F"), None, "metaclass kwarg -> no binding");
        assert_eq!(base_of("G"), None, "no superclasses -> no binding");
    }
}
