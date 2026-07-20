//! L2.4 — compile the vendored PL/SQL external scanner. The pinned `tree-sitter-plsql` git dep
//! ships `scanner.c` but its build.rs never compiles it (upstream template bug), leaving
//! `tree_sitter_plsql_external_scanner_*` undefined at link time. See
//! vendor/tree-sitter-plsql-scanner/README.md for provenance and the re-vendor rule.

fn main() {
    let dir = std::path::Path::new("vendor/tree-sitter-plsql-scanner");
    cc::Build::new()
        .include(dir)
        .flag_if_supported("-Wno-unused-parameter")
        .file(dir.join("scanner.c"))
        .compile("tree-sitter-plsql-scanner");
    println!("cargo:rerun-if-changed=vendor/tree-sitter-plsql-scanner/scanner.c");
}
