# tree-sitter-plsql external scanner (vendored shim)

`scanner.c` + `tree_sitter/` headers copied verbatim from
https://github.com/njank/tree-sitter-plsql at rev `cc92e32c49f31b5009238b7e82e2121ba92e53e9`
(MIT — license declared in that repo's Cargo.toml/package.json/tree-sitter.json).

Why this exists: maple depends on that repo as a pinned git dependency for the PL/SQL grammar
(`parser.c`), but its `bindings/rust/build.rs` never compiles `src/scanner.c` (the template's
scanner block is still commented out), so the `tree_sitter_plsql_external_scanner_*` symbols are
missing at link time. maple's own `build.rs` compiles this copy to fill them in.

IMPORTANT: if the git pin in `Cargo.toml` ever moves, re-copy `scanner.c` (and headers) from the
same rev — parser and scanner must come from the same grammar build. Delete this directory if
upstream fixes its build.rs.
