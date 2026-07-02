#!/usr/bin/env python3
"""
S0.2 harness for `maple` — assemble a depth-1 context bundle for one symbol.  THROWAWAY.

Prototypes maple's `bundle` command in Python so you can measure SC-4 *before* building the Rust core:
produce a byte-sized bundle for a real target symbol, feed it to the ≤32K tier through Bob, and A/B it
against feeding the whole file. Prints the bundle as JSON (D4 schema); prints a bundle-vs-file token
comparison to stderr.

    python3 s0_2_bundle.py /path/to/repo --symbol my_function [--budget 16000] > bundle.json
    python3 s0_2_bundle.py --selfcheck

# ponytail: name-based edges (reuses the rate-spike resolver). Method-call callers are flagged
#           `ambiguous` — exactly the 86%-ambiguity S0.1 found, and what the real F4 resolver fixes.
#           Depth-1 only. Token count is chars/4 approx (D5). Good enough to A/B; not the Rust tool.

Requires: pip install "tree-sitter>=0.23" tree-sitter-python
"""
from __future__ import annotations
import argparse, json, sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from s0_resolution_rates import _make_query, _run_query, SKIP_DIRS, MAX_BYTES  # reuse shims

APPROX_CHARS_PER_TOK = 4  # D5: approximate, conservative. Relative size is what the A/B needs.
def approx_tokens(text: str) -> int:
    return (len(text) + APPROX_CHARS_PER_TOK - 1) // APPROX_CHARS_PER_TOK

def _py():
    import tree_sitter_python as tsp
    from tree_sitter import Language, Parser
    lang = Language(tsp.language())
    defq = _make_query(lang, "(function_definition) @fn")
    callq = _make_query(lang, """
        (call function: (identifier) @call.func)
        (call function: (attribute attribute: (identifier) @call.method))
    """)
    return Parser(lang), defq, callq

def _nearest_fn_name(node):
    p = node.parent
    while p is not None:
        if p.type == "function_definition":
            n = p.child_by_field_name("name")
            return n.text.decode() if n else "<anon>"
        p = p.parent
    return "<module>"

def _sig(fn_node) -> str:
    # first line of the def header (multi-line sigs truncate — fine for a spike)
    return fn_node.text.decode("utf8", "replace").split("\n", 1)[0].rstrip()

def index(root: Path):
    parser, defq, callq = _py()
    defs: dict[str, list[dict]] = {}          # name -> [ {file,start,end,sig,body} ]
    edges: list[dict] = []                     # {caller, callee, file, line, snippet}
    files = 0
    for path in root.rglob("*.py"):
        if any(part in SKIP_DIRS for part in path.parts) or path.stat().st_size > MAX_BYTES:
            continue
        try:
            src = path.read_bytes(); tree = parser.parse(src)
        except Exception as e:
            print(f"  ! parse failed {path}: {e}", file=sys.stderr); continue
        lines = src.decode("utf8", "replace").splitlines()
        rel = str(path.relative_to(root))
        for _, fn in _run_query(defq, tree.root_node):
            nm = fn.child_by_field_name("name")
            if not nm: continue
            defs.setdefault(nm.text.decode(), []).append({
                "file": rel, "start": fn.start_point[0] + 1, "end": fn.end_point[0] + 1,
                "sig": _sig(fn), "body": fn.text.decode("utf8", "replace")})
        for _, ident in _run_query(callq, tree.root_node):
            line = ident.start_point[0] + 1
            snippet = "\n".join(lines[max(0, line - 3): line + 2])
            edges.append({"caller": _nearest_fn_name(ident), "callee": ident.text.decode(),
                          "file": rel, "line": line, "snippet": snippet})
        files += 1
    return files, defs, edges

def resolution_of(name, defs):
    n = len(defs.get(name, []))
    return "exact" if n == 1 else ("ambiguous" if n > 1 else "external")

def build_bundle(root: Path, symbol: str, budget: int):
    files, defs, edges = index(root)
    targets = defs.get(symbol, [])
    if not targets:
        sys.exit(f"symbol not found as a function def: {symbol!r} (indexed {len(defs)} names in {files} files)")
    target = targets[0]
    target_ambiguous = len(targets) > 1

    callees, seen = [], set()
    for e in edges:
        if e["caller"] == symbol and e["callee"] not in seen:
            seen.add(e["callee"])
            res = resolution_of(e["callee"], defs)
            d = defs.get(e["callee"], [{}])[0]
            callees.append({"name": e["callee"], "resolution": res,
                            "signature": d.get("sig"), "file": d.get("file"), "line": d.get("start")})
    callers = [{"enclosing_fn": e["caller"], "call_site": {"file": e["file"], "line": e["line"],
                "snippet": e["snippet"]},
               "resolution": "ambiguous" if target_ambiguous else "exact"}
              for e in edges if e["callee"] == symbol]

    bundle = {
        "target": {"fq_name": symbol, "file": target["file"],
                   "span": [target["start"], target["end"]], "signature": target["sig"],
                   "body": target["body"]},
        "callees": callees,
        "callers": callers,
        "report": {
            "budget": budget,
            "ambiguous": sorted({c["name"] for c in callees if c["resolution"] == "ambiguous"}
                                | ({symbol} if target_ambiguous else set())),
            "external": sorted({c["name"] for c in callees if c["resolution"] == "external"}),
        },
        "meta": {"depth": 1, "tokenizer": "approx(chars/4)", "note": "throwaway S0.2 prototype"},
    }
    tok = approx_tokens(json.dumps(bundle))
    bundle["report"]["token_count"] = tok
    bundle["report"]["over_budget"] = tok > budget          # D5: signal, never silent trim
    # A/B baseline: the whole file the target lives in
    file_text = (root / target["file"]).read_text("utf8", "replace")
    return bundle, tok, approx_tokens(file_text)

def selfcheck():
    import tempfile, textwrap
    with tempfile.TemporaryDirectory() as d:
        (Path(d) / "m.py").write_text(textwrap.dedent("""
            def helper(x): return x + 1
            def target(y):
                return helper(y)
            def caller():
                return target(41)
        """))
        b, tok, ftok = build_bundle(Path(d), "target", 16000)
        assert b["target"]["fq_name"] == "target"
        assert any(c["name"] == "helper" for c in b["callees"]), b["callees"]
        assert any(c["enclosing_fn"] == "caller" for c in b["callers"]), b["callers"]
        assert tok > 0 and ftok > 0
        print("selfcheck OK  (callees:", [c["name"] for c in b["callees"]],
              "callers:", [c["enclosing_fn"] for c in b["callers"]], ")")

def main():
    ap = argparse.ArgumentParser(description="maple S0.2 depth-1 bundle harness (throwaway)")
    ap.add_argument("repo", nargs="?"); ap.add_argument("--symbol")
    ap.add_argument("--budget", type=int, default=16000)
    ap.add_argument("--selfcheck", action="store_true")
    a = ap.parse_args()
    if a.selfcheck: selfcheck(); return
    if not (a.repo and a.symbol): ap.error("need <repo> and --symbol, or --selfcheck")
    bundle, tok, ftok = build_bundle(Path(a.repo).resolve(), a.symbol, a.budget)
    print(json.dumps(bundle, indent=2))
    saved = f"{100*(1-tok/ftok):.0f}%" if ftok else "n/a"
    print(f"\n[A/B] bundle ~{tok} tok  vs  whole-file ~{ftok} tok  ->  {saved} smaller  "
          f"(budget {a.budget}, over={bundle['report']['over_budget']}, "
          f"callers={len(bundle['callers'])}, callees={len(bundle['callees'])})", file=sys.stderr)

if __name__ == "__main__":
    main()
